//! The uniform lane suite: every lane, the same assertions.
//!
//! This is what gives "all lanes expose the same interface" teeth. Three outcome
//! classes keep red meaningful:
//!
//! - [`Applicability::Required`] — the lane MUST implement this. Red is a bug.
//! - [`Applicability::NotApplicable`] — structurally impossible. Red would be
//!   the lane claiming a capability it cannot have.
//! - [`Applicability::KnownGap`] — should work, does not yet. This is the to-do
//!   list, and it is asserted to be EXACTLY the set below, so closing a gap
//!   without updating the grid fails the build.
//!
//! The grid below is the machine-readable form of the "unbuilt today" column in
//! `doc/tbd/provider-architecture/04-stage1-plan.md`.

use crate::lane::{Lane, Mode};

/// Every operation the suite covers, by the name used in the grid.
///
/// **This list is the grid's spine** — [`known_gaps`] walks it and
/// `the_known_gaps_are_exactly_the_documented_ones` asserts the result, so an op
/// missing from here is an op no lane is asserted to owe. `resolved` was such an
/// op: the commit that added `LaneOps::resolved` wired the trait, the impl, the
/// fixture and the wire frame, and left the grid at ten. Under-coverage from an
/// omission is silent by construction, which is why the length is annotated and
/// the count below is checked against [`crate::contract::LaneOps`]'s own method
/// list in prose.
pub const OPS: [&str; 12] = [
    "start",
    "wake",
    "kill",
    "list",
    "health",
    "receive_path",
    "deliver",
    "await_terminal",
    "await_idle",
    "recover",
    "resolved",
    "attach",
];

/// Whether a lane owes an implementation of an op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applicability {
    Required,
    NotApplicable { reason: &'static str },
    KnownGap { detail: &'static str },
}

/// THE grid. What each lane owes for each op.
///
/// Anything not named here is [`Applicability::Required`].
pub fn applicability(lane: Lane, op: &str) -> Applicability {
    use Applicability::*;

    match (lane.harness, lane.mode, op) {
        // --- structural: no terminal to attach to -------------------------
        // RULED (gap J), then NARROWED — 2026-08-17.
        //
        // The ruling was that attach is structurally absent for EVERY daemon lane,
        // and its reasoning was sound: promoting the codex viewer into the
        // contract would oblige every daemon lane to grow a viewer nobody asked
        // for. `codex/app-server` obliges none of them. It is a lane that exists
        // PRECISELY to be attachable — its residence is an app server a second
        // client can join — so it answers `Required` while `codex/daemon`,
        // `pi/daemon` and both ACP lanes keep this arm unchanged.
        //
        // Note the arm below is still a `Mode::Daemon` wildcard, not a
        // hand-listed set: `Mode::AppServer` is a different mode, so the app-server
        // lane never reaches here and the other four are still caught by shape
        // rather than by enumeration.
        (_, Mode::Daemon, "attach") => NotApplicable {
            reason: "a daemon-hosted session has no terminal of its own; drive it with send",
        },

        // --- the verified bugs (doc/tbd/provider-architecture/03) ---------
        //
        // BUG 1 (pi/mux-pane health) is CLOSED — stage-2 phase 1. Its blocker was
        // "gather_pi gates on endpoint.is_some(); a pi TUI row has no endpoint, so
        // the join falls back to Idle unconditionally". `crate::lane_read` keys
        // health on the LANE, so pi/mux-pane reaches the endpoint-free
        // `pi::session::derive_status` and reports `HealthSource::TranscriptTail`
        // — a source, where before there was none. The row is gone from this grid
        // in the same change that closed it, because the gap list is asserted
        // exactly.
        // BUG 2 (pi/mux-pane wake) is CLOSED. Its detail named the defect
        // precisely: "RealWaker::wake has no (\"pi\", Some(MuxPane)) arm, so qd
        // send revives a cold pi TUI through the daemon path" — `RealWaker`'s
        // `("pi", _)` arm called `run_pi_resume`, the RESIDENT revive, for a row
        // that has no resident. `RealWaker` is DELETED: `qd send` now delivers
        // through [`crate::contract::LaneOps::deliver`], which performs its own
        // wake through [`crate::contract::LaneOps::wake`], whose `(Pi, Pane)` arm
        // drives `provider::pi::pane::{plan_pi_tui, revive_pi_tui}` — the TUI
        // revive, pinned total by `lanes::tests::wake_is_total_for_every_lane`.
        // The gap is gone from this grid in the same change that closed it,
        // because the gap list is asserted exactly.

        // `start` appears for no lane, and that is now a statement: it is
        // Required everywhere and, as of stage-2 phase 3, implemented on all
        // seven. What made it a gap was routing — a five-arm ordered if-chain in
        // `lifecycle::run_new` whose ordering was comment-enforced only — and
        // that is gone rather than moved: `Lane::for_create` plus an exhaustive
        // `match (harness, mode)` in `crate::lanes`, with the total input table
        // pinned by `lane::tests::start_routing_is_total_over_every_real_input`.
        _ => Required,
    }
}

/// The gaps, as a flat list — the to-do list this stage produces.
pub fn known_gaps() -> Vec<(Lane, &'static str, &'static str)> {
    let mut out = Vec::new();
    for lane in Lane::ALL {
        for op in OPS {
            if let Applicability::KnownGap { detail } = applicability(lane, op) {
                out.push((lane, op, detail));
            }
        }
    }
    out
}

// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // `Harness` is a TEST-only name here now: the grid stopped naming a harness
    // when BUG 2 closed and its `(Harness::Pi, Mode::Pane, "wake")` row went.
    use crate::lane::Harness;
    use crate::contract::SessionStatus;
    use crate::contract::*;
    use crate::fixture::FixtureLane;
    use std::path::PathBuf;

    fn start_one(l: &FixtureLane) -> SessionId {
        l.start(&StartRequest {
            cwd: PathBuf::from("/tmp/x"),
            name: "s".into(),
            ..Default::default()
        })
        .expect("fixture start")
        .id
        // The fixture identifies from birth; two REAL lanes cannot (see
        // `SessionHandle::id`), which is why the field is an Option and why the
        // suite unwraps it HERE rather than the contract pretending.
        .expect("the fixture lane is identified at create")
    }

    // --- the grid itself --------------------------------------------------

    #[test]
    fn every_lane_has_a_verdict_for_every_op() {
        for lane in Lane::ALL {
            for op in OPS {
                let _ = applicability(lane, op);
            }
        }
    }

    /// The to-do list is EXACTLY this — and it is now EMPTY. Closing a gap
    /// without updating the grid fails here, which is the point: the grid is the
    /// tracker, so an empty one is a claim, not an absence of bookkeeping.
    ///
    /// The three that were here, and what closed each:
    ///   - `pi/daemon:attach_plan` — gap J RULED attach structurally absent for
    ///     every daemon lane, which makes it `NotApplicable` rather than a debt.
    ///   - `pi/mux-pane:health` (BUG 1) — stage-2 phase 1 gave the lane a real
    ///     endpoint-free source (`crate::lane_read`'s transcript tail).
    ///   - `pi/mux-pane:wake` (BUG 2) — `send_unified::RealWaker`, whose missing
    ///     `("pi", Some(MuxPane))` arm WAS the bug, is deleted; `qd send` reaches
    ///     `LaneOps::wake` through `LaneOps::deliver`, and that arm exists.
    #[test]
    fn the_known_gaps_are_exactly_the_documented_ones() {
        let gaps: Vec<String> = known_gaps()
            .into_iter()
            .map(|(lane, op, _)| format!("{lane}:{op}"))
            .collect();
        let none: [String; 0] = [];
        assert_eq!(
            gaps, none,
            "known gaps drifted from doc/tbd/provider-architecture/03-verified-bugs.md — \
             update the grid in the same change that closes or opens one"
        );
    }

    /// **BUG 2, verified CLOSED rather than merely delisted.**
    ///
    /// The bug was a ROUTE: qd's own waker had `("pi", _)` as one arm, so a cold
    /// pi row in a mux PANE was revived through `run_pi_resume` — the RESIDENT
    /// revive, `--load-session` against a ws endpoint a TUI row does not have.
    /// Deleting the row from the grid above proves nothing on its own, so this
    /// asserts the two facts that actually close it:
    ///
    ///  1. pi's two lanes are DISTINCT lanes, so a router keyed on `(harness,
    ///     mode)` cannot collapse them the way one keyed on a provider string
    ///     did; and
    ///  2. `wake` is `Required` — not a gap, not `NotApplicable` — for pi/pane,
    ///     which is the grid's own statement that the arm must exist.
    ///
    /// That the arm is REAL (delegation, not a `todo!()`) is pinned one crate
    /// module over by `lanes::tests::wake_is_total_for_every_lane`, which drives
    /// every lane's `wake` to its row lookup; and that it drives the TUI revive
    /// rather than the resident one is pinned by the `(Pi, Pane)` arm's own
    /// `plan_pi_tui`/`revive_pi_tui` calls.
    #[test]
    fn bug_2_is_closed_pi_pane_owes_a_wake_of_its_own() {
        let pane = Lane::new(Harness::Pi, Mode::Pane).expect("pi has a pane lane");
        let daemon = Lane::new(Harness::Pi, Mode::Daemon).expect("pi has a daemon lane");
        assert_ne!(
            pane, daemon,
            "the two pi lanes must be distinct, or `(harness, mode)` routing cannot \
             tell a TUI revive from a resident one — which IS the bug"
        );
        assert_eq!(
            applicability(pane, "wake"),
            Applicability::Required,
            "pi/mux-pane owes a wake of its own; a KnownGap here would be BUG 2 reopened"
        );
    }

    #[test]
    fn unattachable_daemon_lanes_declare_attach_impossible_not_a_gap() {
        for lane in Lane::ALL
            .iter()
            .filter(|l| l.is_daemon() && !l.is_app_server())
        {
            assert!(
                matches!(
                    applicability(*lane, "attach"),
                    Applicability::NotApplicable { .. }
                ),
                "{lane} attach must be NotApplicable, never a gap to close"
            );
        }
    }

    /// The narrowing of ruling J, asserted from the other side: the one daemon
    /// lane that DOES owe an attach.
    ///
    /// A `NotApplicable` here would mean the lane had been folded back under the
    /// wildcard and its whole reason for existing had quietly evaporated — the
    /// session would still start and still deliver, so nothing else would notice.
    #[test]
    fn the_app_server_lane_owes_an_attach() {
        let lane = Lane::new(Harness::Codex, Mode::AppServer).expect("the app-server lane");
        assert_eq!(
            applicability(lane, "attach"),
            Applicability::Required,
            "codex/app-server exists to be attachable; that is the whole lane"
        );
    }

    #[test]
    fn pane_lanes_must_all_support_attach() {
        for lane in Lane::ALL.iter().filter(|l| l.is_pane()) {
            assert_eq!(
                applicability(*lane, "attach"),
                Applicability::Required,
                "{lane} is pane-hosted; attach is the point of a pane"
            );
        }
    }

    // --- the uniform behavioural suite, run against the fixture -----------

    #[test]
    fn every_lane_shape_satisfies_the_suite() {
        for lane in Lane::ALL {
            let l = FixtureLane::new(lane);
            let id = start_one(&l);

            assert_eq!(l.lane(), lane);
            let listed = l.list().unwrap();
            assert_eq!(listed.sessions.len(), 1, "{lane}: started session must list");
            assert!(
                listed.degraded.is_empty(),
                "{lane}: an in-memory store cannot be refused"
            );

            let h = l.health(&id).unwrap();
            assert_eq!(h.status, SessionStatus::Idle, "{lane}");

            let r = l
                .deliver(
                    &id,
                    &Message {
                        id: MessageId("m-fixture".into()),
                        text: "hi".into(),
                        from: None,
                    },
                    &DeliverPolicy::default(),
                )
                .unwrap();
            assert!(r.accepted, "{lane}");
            assert_eq!(r.terminal, TerminalExpectation::Pending, "{lane}");
            assert_eq!(
                l.await_terminal(&id, &r.message_id, 1_000).unwrap(),
                Terminal::Seen,
                "{lane}"
            );
            // `resolved` is the same question WITHOUT waiting, so it must answer
            // the same terminal and answer it as a `Some`. Driving it beside
            // `await_terminal` is what stops the two drifting: a `resolved` that
            // reported `None` for a resolved send would send the sweep to
            // `recover`, which is the outcome the method exists to prevent.
            assert_eq!(
                l.resolved(&LedgerAddress::session(id.clone()), &r.message_id)
                    .unwrap(),
                Some(Terminal::Seen),
                "{lane}: resolved must see the terminal await_terminal just saw"
            );
            // `await_idle` has a different SUBJECT (the session, not the message),
            // so it is asserted on the session's own state: this one is live and
            // has no turn in flight, and the answer that carries that is
            // `IdleAtEntry` — never `WentIdle`, which would claim a busy→idle
            // transition nothing observed.
            assert_eq!(
                l.await_idle(&id, 1_000).unwrap(),
                TurnState::IdleAtEntry,
                "{lane}: an already-idle session is IdleAtEntry, never WentIdle"
            );

            assert_eq!(
                l.kill(&id).unwrap().outcome.reaped,
                Confirmation::Yes,
                "{lane}"
            );
            assert!(
                l.list().unwrap().sessions.is_empty(),
                "{lane}: killed session must not list"
            );
        }
    }

    #[test]
    fn unknown_session_is_not_found_never_a_silent_success() {
        let l = FixtureLane::new(Lane::ALL[0]);
        let ghost = SessionId("nope".into());
        assert!(matches!(l.health(&ghost), Err(LaneError::NotFound { .. })));
        assert!(matches!(l.kill(&ghost), Err(LaneError::NotFound { .. })));
        assert!(matches!(l.attach(&ghost), Err(LaneError::NotFound { .. })));
    }

    /// Stage-1 decision 1: wake is EXPLICIT in the request. With it off, a cold
    /// target is never a surprise revive.
    ///
    /// It is also not a REFUSAL, and that half changed. `deliver` used to answer
    /// [`LaneError::Cold`] here; it now ATTEMPTS and lets whatever takes the
    /// message report. The reason is written out on
    /// [`crate::contract::LaneOps::deliver`]: two components answer "is this live"
    /// differently on purpose, the real lane's answer is a PROJECTION over
    /// `(pid, start_time)` that qd's live path deliberately does not share, and a
    /// refusal off that projection would hand qd's live path a class it has no
    /// rendering for. `wake_if_cold: false` therefore means "do not revive", not
    /// "refuse if not live".
    ///
    /// What this pins is the surviving half, which is the one the flag was added
    /// for: no revive happened. The session is STILL COLD afterwards, and the
    /// receipt says [`Confirmation::No`] so qd stamps no `queued`.
    #[test]
    fn deliver_with_wake_if_cold_false_attempts_and_never_revives() {
        let l = FixtureLane::new(Lane::ALL[0]);
        let id = start_one(&l);
        l.sessions_set_cold(&id);
        let r = l
            .deliver(
                &id,
                &Message {
                    id: MessageId("m-fixture".into()),
                    text: "x".into(),
                    from: None,
                },
                &DeliverPolicy {
                    wake_if_cold: false,
                    budget_ms: 10,
                    ..Default::default()
                },
            )
            .expect("wake_if_cold: false must ATTEMPT, never refuse off liveness");
        assert_eq!(
            r.woke,
            Confirmation::No,
            "nothing was revived, so qd stamps no `queued`"
        );
        assert_eq!(
            l.health(&id).unwrap().status,
            SessionStatus::Cold,
            "the session is STILL COLD — `wake_if_cold: false` means no revive, and \
             that is the half of this flag that did not change"
        );
    }

    #[test]
    fn deliver_wakes_when_asked() {
        let l = FixtureLane::new(Lane::ALL[0]);
        let id = start_one(&l);
        l.sessions_set_cold(&id);
        let r = l
            .deliver(
                &id,
                &Message {
                    id: MessageId("m-fixture".into()),
                    text: "x".into(),
                    from: None,
                },
                &DeliverPolicy {
                    wake_if_cold: true,
                    budget_ms: 10,
                    ..Default::default()
                },
            )
            .expect("wake_if_cold must revive");
        assert!(r.accepted);
        // The receipt REPORTS the wake. This is the half of the phase-2 repair qd
        // reads: `queued` is stamped retrospectively off this field, so a lane
        // that woke silently would cost the ledger a row. See `Receipt::woke`.
        assert_eq!(r.woke, Confirmation::Yes, "a wake must be reported back");
    }

    #[test]
    fn a_delivery_that_needed_no_wake_says_so() {
        // The other side of the same pin: a LIVE target must answer
        // `Confirmation::No`, because qd stamps `queued` on anything that is not
        // `No` and the live funnel has no `queued` row.
        let l = FixtureLane::new(Lane::ALL[0]);
        let id = start_one(&l);
        let r = l
            .deliver(
                &id,
                &Message {
                    id: MessageId("m-fixture".into()),
                    text: "x".into(),
                    from: None,
                },
                &DeliverPolicy::default(),
            )
            .unwrap();
        assert_eq!(r.woke, Confirmation::No);
    }

    #[test]
    fn a_lane_that_cannot_confirm_says_so_at_receipt_time() {
        // The pi-first-turn case: delivered, but no terminal will ever come. The
        // caller must learn that from the RECEIPT, not by blocking to timeout.
        let l = FixtureLane::new(Lane::ALL[0]).with_terminal_expectation(
            TerminalExpectation::Unavailable {
                reason: "pi has written nothing to disk before its first assistant turn".into(),
            },
        );
        let id = start_one(&l);
        let r = l
            .deliver(
                &id,
                &Message {
                    id: MessageId("m-fixture".into()),
                    text: "x".into(),
                    from: None,
                },
                &DeliverPolicy::default(),
            )
            .unwrap();
        assert!(r.accepted, "unavailable terminal is not a delivery failure");
        assert!(matches!(
            r.terminal,
            TerminalExpectation::Unavailable { .. }
        ));
    }

    #[test]
    fn the_suite_discriminates() {
        // A lane wired to fail must actually go red — otherwise green means nothing.
        let l = FixtureLane::new(Lane::ALL[0]).with_failure(
            "health",
            LaneError::Transport {
                detail: "boom".into(),
            },
        );
        let id = start_one(&l);
        assert!(matches!(l.health(&id), Err(LaneError::Transport { .. })));
    }

    // --- guard 1: every DTO survives serde -------------------------------

    #[test]
    fn every_dto_round_trips_through_json() {
        macro_rules! rt {
            ($v:expr) => {{
                let v = $v;
                let j = serde_json::to_string(&v).expect("serialize");
                let back = serde_json::from_str(&j).expect("deserialize");
                assert_eq!(v, back, "round-trip failed for {}", stringify!($v));
            }};
        }

        rt!(SessionId("s".into()));
        rt!(MessageId("m".into()));
        // Both halves of the ledger address, separately AND together: the pair is
        // the point (see `LedgerAddress`), so a round-trip over only the session
        // form would not notice the `name` half being dropped on the wire.
        rt!(LedgerAddress::session(SessionId("s".into())));
        rt!(LedgerAddress::byname("wk"));
        rt!(LedgerAddress {
            session: Some(SessionId("s".into())),
            name: Some("wk".into()),
        });
        rt!(StartRequest {
            cwd: PathBuf::from("/tmp"),
            name: "n".into(),
            model: Some("m".into()),
            resume: Some(SessionId("r".into())),
            passthrough: vec!["--x".into()],
            prompt: Some("hello".into()),
            await_relay: false,
            env: vec![("ANTHROPIC_BASE_URL".into(), "https://x".into())],
            env_unset: vec!["ANTHROPIC_API_KEY".into()],
            render: crate::launch::RenderMode::AltScreen,
        });
        // The default is a DTO too, and the field it exists for is `await_relay`
        // — `#[derive(Default)]` would have answered `false` and silently made
        // every defaulted request a `--no-await-relay` one.
        rt!(StartRequest::default());
        assert!(
            StartRequest::default().await_relay,
            "relay-await is DEFAULT-ON for start; --no-await-relay is the opt-out"
        );
        rt!(SessionHandle {
            id: Some(SessionId("s".into())),
            qd_id: Some("ab12cd34".into()),
            pid: Some(1),
            started_at_ms: Some(2),
            // Both are POPULATED here on purpose, for the same reason
            // `ReapObservations` below is: a round trip over the empty defaults
            // would prove nothing about the two fields a pane create actually
            // fills, and `notes` in particular is the one the acp arm used to
            // drop on the floor.
            socket_dir: Some(std::path::PathBuf::from("/tmp/zmx-501")),
            notes: vec!["relay sidecar not observed".into()],
        });
        // The shape two of the seven lanes actually return at create: a stable
        // qd id, and NO provider id yet.
        rt!(SessionHandle {
            id: None,
            qd_id: Some("ab12cd34".into()),
            pid: None,
            started_at_ms: None,
            socket_dir: None,
            notes: Vec::new(),
        });
        rt!(KillOutcome {
            reaped: Confirmation::Yes,
            tombstoned: Confirmation::Unknown
        });
        // The observation half. Every field is populated on purpose: a
        // round-trip over `..Default::default()` would prove nothing about the
        // six fields the flattened return type used to drop.
        rt!(ReapObservations {
            notes: vec!["\"wk\": recorded pid 4242 belongs to a different process".into()],
            failures: vec!["zmx session \"wk\"".into()],
            nothing_to_kill: false,
            survivor_dir: Some("/tmp/zmx-501".into()),
            zmx_name: Some("wk".into()),
            pid: 4242,
            zmx_dir_unconfirmed: true,
            was_alive: Some(false),
        });
        rt!(ReapObservations::default());
        rt!(KillReport {
            outcome: KillOutcome {
                reaped: Confirmation::Unknown,
                tombstoned: Confirmation::Unknown,
            },
            observed: ReapObservations {
                pid: 4242,
                was_alive: Some(true),
                ..Default::default()
            },
        });
        // The wake half. Both states, because the DISTINCTION is the payload:
        // an `AlreadyRunning` that deserialized as `Revived` would be a caller
        // reporting a revive that never happened.
        rt!(WakeState::AlreadyRunning);
        rt!(WakeState::Revived);
        rt!(Resident {
            pid: 4242,
            endpoint: "ws://127.0.0.1:18951".into(),
        });
        rt!(PaneHandle {
            zmx_name: "wk".into(),
            socket_dir: "/tmp/zmx-501".into(),
        });
        rt!(WakeOutcome {
            state: WakeState::AlreadyRunning,
            handle: SessionHandle {
                id: Some(SessionId("s".into())),
                qd_id: None,
                pid: Some(4242),
                started_at_ms: Some(2),
                socket_dir: None,
                notes: Vec::new(),
            },
            resident: None,
            pane: None,
        });
        rt!(WakeOutcome {
            state: WakeState::Revived,
            handle: SessionHandle {
                id: Some(SessionId("s".into())),
                qd_id: None,
                pid: Some(4242),
                started_at_ms: Some(2),
                socket_dir: None,
                notes: Vec::new(),
            },
            resident: Some(Resident {
                pid: 4242,
                endpoint: "ws://127.0.0.1:18951".into(),
            }),
            pane: None,
        });
        rt!(WakeOutcome {
            state: WakeState::Revived,
            handle: SessionHandle {
                id: Some(SessionId("s".into())),
                qd_id: None,
                pid: None,
                started_at_ms: None,
                socket_dir: None,
                notes: Vec::new(),
            },
            resident: None,
            pane: Some(PaneHandle {
                zmx_name: "wk".into(),
                socket_dir: "/tmp/zmx-501".into(),
            }),
        });
        rt!(SessionSummary {
            id: SessionId("s".into()),
            provider: "codex".into(),
            name: None,
            cwd: Some("/tmp".into()),
            status: SessionStatus::Busy,
            turns: 3,
            tokens: 4,
            last_active_ms: Some(5),
            git_branch: Some("main".into()),
        });
        rt!(Health {
            status: SessionStatus::Idle,
            source: HealthSource::TranscriptTail,
            observed_at_ms: Some(6),
        });
        rt!(Message {
            id: MessageId("m-fixture".into()),
            text: "t".into(),
            from: Some("f".into())
        });
        rt!(DeliverPolicy {
            wake_if_cold: false,
            budget_ms: 7,
            render: crate::launch::RenderMode::AltScreen,
        });
        // The one DTO field that is another module's type. It round-trips as the
        // config file's own token (`alt-screen`), which is what makes carrying it
        // as data rather than reading it in qw a lossless move.
        rt!(crate::launch::RenderMode::Inline);
        rt!(crate::launch::RenderMode::AltScreen);
        rt!(Degradation {
            source: "~/.claude/projects".into(),
            detail: "Permission denied (os error 13)".into(),
        });
        rt!(Listing {
            sessions: vec![],
            degraded: vec![Degradation {
                source: "s".into(),
                detail: "d".into()
            }],
        });
        // The three receive-path answers. `Undetermined` is the one the type
        // exists for — it is the only channel `refused{receive-path-undetermined}`
        // has, and a round-trip that dropped its `reason` would strip the OS error
        // qd prints as its evidence.
        rt!(ReceivePath::Available);
        rt!(ReceivePath::None {
            reason: "neither a recorded relay port nor a joined mux pane".into()
        });
        rt!(ReceivePath::Undetermined {
            reason: "the process read was refused (Operation not permitted)".into()
        });
        rt!(TerminalExpectation::Pending);
        rt!(TerminalExpectation::Unavailable { reason: "r".into() });
        rt!(Receipt {
            message_id: MessageId("m".into()),
            accepted: true,
            terminal: TerminalExpectation::Pending,
            woke: Confirmation::Unknown,
        });
        rt!(Terminal::Seen);
        rt!(Terminal::NotDelivered { reason: "r".into() });
        rt!(Terminal::Mismatch);
        rt!(Terminal::TimedOut);
        rt!(Terminal::Undetermined { reason: "r".into() });
        // Every `TurnState`. The entry/loop pair is the point (see `TurnState`):
        // a round-trip that collapsed `IdleAtEntry` into `WentIdle` would make
        // `qd wait` print an orphaned ` done` for a session it never waited on.
        rt!(TurnState::IdleAtEntry);
        rt!(TurnState::WentIdle);
        rt!(TurnState::BudgetElapsed);
        rt!(TurnState::SessionExited);
        rt!(TurnState::ChannelClosed);
        rt!(TurnState::TurnFailed {
            detail: "internalError".into()
        });
        rt!(TurnState::Undetermined { reason: "r".into() });
        rt!(Confirmation::Yes);
        rt!(Confirmation::No);
        rt!(Confirmation::Unknown);
        rt!(LaneError::NotSupported {
            op: "attach".into(),
            reason: "r".into()
        });
        rt!(LaneError::NotFound {
            id: SessionId("s".into())
        });
        rt!(LaneError::Cold {
            id: SessionId("s".into())
        });
        rt!(LaneError::Transport { detail: "d".into() });
        // Both attribution shapes. The flag is the difference between
        // `qd resume: ERROR: …` and `ERROR: …`, so a round-trip that lost it
        // would double-attribute a line the core already wrote.
        rt!(LaneError::WakeFailed {
            detail: "d".into(),
            exit_code: 1,
            self_attributed: false
        });
        rt!(LaneError::WakeFailed {
            detail: "ERROR: the session's recorded directory no longer exists".into(),
            exit_code: 1,
            self_attributed: true
        });
        rt!(LaneError::Refused { detail: "d".into() });
        // The create half. Both the boot-timeout shape and the ordinary one, and
        // the SELECTOR exit code (2, not 1) — each of the three fields is a fact a
        // caller cannot re-derive, and each was lost by an earlier signature: the
        // exit code by flattening every create failure to 1, and the phase by
        // string-matching it back out of `detail`.
        rt!(LaneError::StartFailed {
            detail: "qd start: name 'x' is already in use by a live session".into(),
            exit_code: 1,
            boot_phase: None,
        });
        rt!(LaneError::StartFailed {
            detail: "qd: invalid QD_MUX value \"nonsense\"".into(),
            exit_code: crate::mux_selector::QD_MUX_INVALID_EXIT,
            boot_phase: None,
        });
        for phase in [
            crate::boot::BootPhase::PidFile,
            crate::boot::BootPhase::Idle,
            crate::boot::BootPhase::Relay,
        ] {
            rt!(LaneError::StartFailed {
                detail: "ERROR: Session \"x\" did not reach idle state within timeout.".into(),
                exit_code: 1,
                boot_phase: Some(phase),
            });
        }
    }

    // --- guard 2: the trait stays serializable ---------------------------

    /// Source-scan the trait body for anything that could never cross a process
    /// boundary. Same idiom as `send_relay.rs`'s `include_str!` structural guard.
    ///
    /// If this fails, do NOT add an exception — the boundary is drawn wrong.
    ///
    /// It now pins the POSITIVE half too (stage-2 phase 2). The render mode is the
    /// case that showed why: before the revision `wake` took only an id, qw could
    /// not read the `render-default` config (it lives behind `dispatch::secrets`,
    /// on qd's side by design), and the three pane arms in `crate::lanes`
    /// hardcoded `RenderMode::default()` — so a user's `alt-screen` setting was
    /// silently dropped. Nothing failed; a signature was simply too narrow to
    /// carry a fact, which is a failure mode a ban-list cannot see. So the guard
    /// asserts the crossing EXISTS and is a value.
    #[test]
    fn the_trait_signature_stays_serializable() {
        let src = include_str!("contract.rs");
        let start = src
            .find("pub trait LaneOps {")
            .expect("LaneOps trait must exist in contract.rs");
        let body = &src[start..];
        let end = body.find("\n}").expect("trait must be closed");
        let body = &body[..end];

        // Strip doc comments — prose may legitimately mention these.
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("///"))
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        for banned in ["&dyn ", "impl ", "Box<dyn", "where ", "|"] {
            assert!(
                !code.contains(banned),
                "LaneOps signature contains {banned:?}, which cannot be serialized.\n\
                 Effects belong behind &self, injected at construction.\n\
                 Offending body:\n{code}"
            );
        }

        // The positive half. Each of these is a fact the interface was once too
        // narrow to carry, pinned so it cannot quietly narrow again.
        assert!(
            code.contains(
                "fn wake(&self, id: &SessionId, render: RenderMode, cwd_override: Option<String>)"
            ),
            "wake must RECEIVE the resolved render mode as a value — qw cannot read \
             `render-default` (it is behind dispatch::secrets, on qd's side), and a \
             wake that defaults it hands an alt-screen user an inline pane. It must \
             ALSO receive `qd resume --cwd`: the lane hardcoded `None` for it, so \
             routing resume through wake would have validated the user's override \
             and then discarded it — the same defect, one field over.\n\
             Body:\n{code}"
        );
        assert!(
            code.contains("-> Result<WakeOutcome, LaneError>"),
            "wake must answer a WakeOutcome. A bare SessionHandle cannot say whether \
             the session was REVIVED or was already running, and the four daemon \
             lanes decide that at an instant no caller can re-observe — so a caller \
             holding only a handle can only guess, and printing \"resumed …\" for a \
             session that was never revived is a false statement of fact.\n\
             Body:\n{code}"
        );
        assert!(
            code.contains("fn kill(&self, id: &SessionId) -> Result<KillReport, LaneError>"),
            "kill must answer a KillReport. KillOutcome's two Confirmations are what a \
             lane can honestly CLAIM; `reaped: No` is the single answer for three \
             distinct exit-1 messages, and the r7 foreign-pid note exists nowhere \
             else in the process. The observation rides alongside the claim.\n\
             Body:\n{code}"
        );
        assert!(
            code.contains("fn start(&self, req: &StartRequest) -> Result<SessionHandle, LaneError>"),
            "start must take the WHOLE request. The five-field version could not carry a \
             create-time prompt, the relay-await decision, the --via env pairs or the render \
             mode — four facts real lanes need and one signature could not express.\n\
             Body:\n{code}"
        );
        assert!(
            code.contains("fn list(&self) -> Result<Listing, LaneError>"),
            "list must answer with a Listing, so a lane that read 40 rows and was \
             REFUSED one store can report BOTH halves. Err() discards the rows it \
             did read; a bare Vec discards the refusal.\n\
             Body:\n{code}"
        );
    }

    // --- guard 3: receive_path is never an INPUT to deliver ---------------

    /// The one discipline that makes [`LaneOps::receive_path`] safe to compose,
    /// enforced mechanically rather than promised in prose.
    ///
    /// `receive_path` answers TOPOLOGY, which is why it does not reopen the race
    /// `deliver`'s atomicity rule guards (that rule protects the STEER-vs-QUEUE
    /// choice, which is turn state, and a stale topology answer is self-correcting
    /// — the carrier dies and `deliver` returns `Transport`, exactly as it does
    /// today when a carrier dies mid-send). That argument holds only while
    /// `deliver` keeps determining the receive path ITSELF: the moment a caller
    /// can hand its earlier answer back in, there IS a time-of-check dependency
    /// for `deliver` to be wrong about, and the whole justification collapses.
    ///
    /// So: `deliver`'s parameter list may never mention [`ReceivePath`]. This is a
    /// source scan for the same reason its two siblings above are — the property
    /// is about a SIGNATURE, and no runtime call can observe a parameter that has
    /// not been added yet.
    ///
    /// MUTATION EVIDENCE: adding `path: &ReceivePath` (or an owned/Option form) to
    /// `fn deliver` reds this.
    #[test]
    fn receive_path_is_never_an_input_to_deliver() {
        let src = include_str!("contract.rs");
        let start = src
            .find("    fn deliver(")
            .expect("LaneOps::deliver must exist in contract.rs");
        let after = &src[start..];
        let end = after
            .find("-> Result<Receipt, LaneError>;")
            .expect("deliver must still answer Result<Receipt, LaneError>");
        let params = &after[..end];

        assert!(
            !params.contains("ReceivePath"),
            "deliver must NEVER accept a ReceivePath. It determines the receive path \
             itself, internally, from its own fresh read — that is what leaves no \
             time-of-check dependency for it to be wrong about, and it is the entire \
             reason `LaneOps::receive_path` is allowed to exist beside an ATOMIC \
             deliver. `receive_path` is for RENDERING a refusal before qd commits an \
             intent record, never for choosing a carrier.\n\
             Offending signature:\n{params}"
        );

        // Positive control: the scan is looking at the real parameter list, so a
        // renamed or restructured `deliver` cannot make the ban vacuously true.
        assert!(
            params.contains("policy: &DeliverPolicy"),
            "the scan lost track of deliver's parameter list — re-derive it.\n\
             Found:\n{params}"
        );
    }
}
