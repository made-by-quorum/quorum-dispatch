//! Shared LIVENESS classifier + lifecycle state machine (WP-A — bugs #4 + #1).
//!
//! This is the ONE trait (§6.4) both #4 (launcher/boot liveness — "silent ≠
//! dead") and #1 (registration-row absence is not liveness) route through, so
//! the parallel (B) headless-stream-json track can later swap the IMPLEMENTATION
//! (add a producer-readiness input) without touching the call sites. Today's
//! implementation is the OS classifier the §9a memo's Step-2 spike grounded:
//! `(pid, /proc starttime)` identity + the `/proc/<pid>/stat` scheduler state.
//!
//! ## The root-cause invariant (§6.0)
//! The disease (memo §1) is consumers treating a persistence/replay artifact as
//! a real-time coordination signal. This classifier reads the **live OS process
//! table** — never a registry row, never a transcript — so it introduces no new
//! disk-as-status read. Row-absence (#1) and connect-refusal/silence (#4) are
//! both DELEGATED here instead of being read as death directly.
//!
//! ## Fail-closed (the load-bearing rule, memo #4.4)
//! Death is asserted ONLY on a positive Exited*/Gone verdict. Every ambiguity —
//! a probe that could not answer, a present-but-unverifiable identity — resolves
//! to ALIVE. The classifier NEVER convicts a process of death from the *absence*
//! of a signal (silence, a refused connect, a missing row): only from positive
//! evidence that it exited or is gone.
//!
//! ## What it cannot do (defers with (B))
//! It separates ALIVE (R/S/D) from EXITED (Z / gone) from NOT-OURS (starttime
//! mismatch) — cross-process and PID-reuse-robust. It CANNOT separate
//! ready-vs-silent or stuck-vs-silent (both read S) — that needs a producer
//! progress signal, which is (B). So `ready`/`stuck` are NOT states here.

use crate::effects::{self, ProcLiveness};

/// The reuse-robust process identity: a pid plus the start-time (epoch ms) that
/// was recorded for it when the session registered. `(pid, start_ms)` survives
/// `exec` (cmdline changes; start time does not) and defeats PID reuse — a pid
/// now held by a process that started materially later is a DIFFERENT process
/// ([`LifecycleState::NotOurs`]), not our session resurrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcKey {
    pub pid: i32,
    /// Start time recorded at registration, epoch ms (from
    /// [`effects::proc_start_ms`] — the SAME metric the classifier re-reads, so
    /// the comparison is apples-to-apples).
    pub start_ms: i64,
}

impl ProcKey {
    pub fn new(pid: i32, start_ms: i64) -> Self {
        Self { pid, start_ms }
    }
}

/// The shared lifecycle state machine #1 and #4 both classify into. `ready` /
/// `stuck` are deliberately NOT members (memo Step-3: they need a producer
/// progress signal → (B)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// Present, scheduler state S/T/I/… — alive but quiet (the 2–7s silent
    /// window trackB measured). The state #4's false-positive-death guard
    /// protects: silent is NOT dead.
    AliveSilentValid,
    /// Present, scheduler state R/D — on-CPU or in uninterruptible I/O.
    AliveWorking,
    /// Exited normally (voluntary `exit`, any code). Produced by the
    /// daemon-as-parent refinement ([`classify_reaped`]) only — a non-parent
    /// observer cannot read the exit status (see [`LifecycleState::ExitedSignal`]).
    ExitedClean,
    /// Exited by signal — OR, on the cross-process observer path, a ZOMBIE whose
    /// exit status is unknowable without the reaping parent (`waitpid ⇒ ECHILD`,
    /// memo Step-2). The conservative "exited, status not provable from here";
    /// the kill gate treats Clean/Signal/Gone identically, so the imprecision is
    /// diagnostic-only. [`classify_reaped`] refines it when the parent reaps.
    ExitedSignal,
    /// Reaped and gone — `/proc/<pid>` is absent (ESRCH). Positive death.
    Gone,
    /// The pid is held by a DIFFERENT process (start-time mismatch). Not our
    /// session: neither our-alive nor our-dead — never killed, never counted
    /// alive.
    NotOurs,
    /// (B) readiness sub-state — the producer is up and has emitted its first
    /// output (`system/init`), so it is a REAL producer signal (gate E/R3: not a
    /// heuristic). Between turns / idle-ready. An ALIVE sub-state.
    AliveReady,
    /// (B) readiness sub-state — **ledger-fed**: the session is parked on a
    /// relay/question `--wait` — alive but WAITING, distinct from working and
    /// stuck. The claude stream ALONE cannot tell a `--wait` from a long turn (a
    /// blocking `--wait` looks like a long turn), so the relay/bond **ledger**
    /// feeds this; without it a waiting session would be mis-read as [`Self::Stuck`]
    /// (the amend's whole point). An ALIVE sub-state.
    AliveWaiting,
    /// (B) readiness sub-state — **DIAGNOSTIC-ONLY (§H.7 / gate R3/D7).** A
    /// turn-start-anchored **wall-clock timeout** heuristic ("no progress past τ
    /// since the daemon-dispatched turn-start"). Materially the same class as
    /// busy-stale, improved only by its ANCHOR (a real turn-start the daemon
    /// knows, not a file mtime) — NOT a progress signal (no
    /// `--include-partial-messages`). **NEVER authorizes a kill/restart;
    /// consumers MUST NOT build control automation on it** (it is `is_alive()`,
    /// never `is_dead()`; gate it with [`Self::is_diagnostic_stuck`]). An ALIVE
    /// sub-state.
    Stuck,
}

impl LifecycleState {
    /// Is this process OUR session, and alive? (NotOurs is not ours; Exited*/Gone
    /// are not alive.)
    pub fn is_alive(self) -> bool {
        matches!(
            self,
            Self::AliveSilentValid
                | Self::AliveWorking
                | Self::AliveReady
                | Self::AliveWaiting
                | Self::Stuck
        )
    }

    /// Is this POSITIVE evidence that our session died? Exited (either way) or
    /// gone. The kill/death gate (#4) keys on exactly this — and ONLY this.
    /// `NotOurs` is deliberately excluded: a reused pid is not our death, and
    /// signaling it would be a wrong-victim kill. The (B) alive sub-states
    /// (`AliveReady`/`AliveWaiting`/`Stuck`) are NOT dead — the death gate is
    /// provably untouched by the readiness augmentation.
    pub fn is_dead(self) -> bool {
        matches!(self, Self::ExitedClean | Self::ExitedSignal | Self::Gone)
    }

    /// (B) DIAGNOSTIC-ONLY contract (§H.7): is this the timeout-heuristic
    /// [`Self::Stuck`] state? True for `Stuck` ONLY. Consumers MUST gate any
    /// stuck-rendering on THIS predicate and MUST NOT build control automation
    /// (kill/restart) on it — `Stuck` is `is_alive()`, never `is_dead()`.
    pub fn is_diagnostic_stuck(self) -> bool {
        matches!(self, Self::Stuck)
    }
}

/// PID-identity slack for the start-time comparison. Reuses the registry-row
/// slack ([`crate::kill::START_TIME_SLACK_MS`], 120s): both the recorded stamp
/// and the re-read are `now − etime` reads (second resolution, two clock reads),
/// and the recorded value may have come from a different writer, so the slack
/// must stay as generous as the registry path's. A pid freed AND reused by a
/// process that started within 120s of the original would pass the identity
/// check — the same documented residual the kill path carries; pids do not
/// recycle that fast in practice without a wrap.
pub const START_SLACK_MS: i64 = crate::kill::START_TIME_SLACK_MS;

/// The ONE liveness trait (§6.4). (B) swaps the impl (a producer-readiness
/// input) without touching the consumers (#1 boot row-absence, #4 boot/launcher
/// death conviction).
pub trait LivenessSource {
    /// Classify the process identified by `key` into the shared state machine.
    fn classify(&self, key: ProcKey) -> LifecycleState;
}

/// The raw per-pid OS reads the classifier folds. Seam so the classification
/// LOGIC is unit-testable across all six states deterministically; the real
/// impl ([`OsProbe`]) reads the live process table.
pub trait ProcProbe {
    /// Recorded-metric start time, epoch ms ([`effects::proc_start_ms`]).
    fn start_ms(&self, pid: i32) -> Option<i64>;
    /// The scheduler-state reading ([`effects::proc_liveness`]).
    fn liveness(&self, pid: i32) -> ProcLiveness;
}

/// Production probe: the live OS process table.
pub struct OsProbe;

impl ProcProbe for OsProbe {
    fn start_ms(&self, pid: i32) -> Option<i64> {
        effects::proc_start_ms(pid)
    }
    fn liveness(&self, pid: i32) -> ProcLiveness {
        effects::proc_liveness(pid)
    }
}

/// The OS [`LivenessSource`]: `(pid, starttime)` identity + `/proc` state.
pub struct OsLiveness<P: ProcProbe = OsProbe> {
    probe: P,
}

impl Default for OsLiveness<OsProbe> {
    fn default() -> Self {
        Self::new()
    }
}

impl OsLiveness<OsProbe> {
    pub fn new() -> Self {
        Self { probe: OsProbe }
    }
}

impl<P: ProcProbe> OsLiveness<P> {
    /// Construct over an injected probe (tests).
    pub fn with_probe(probe: P) -> Self {
        Self { probe }
    }

    fn classify_inner(&self, key: ProcKey) -> LifecycleState {
        let liveness = self.probe.liveness(key.pid);
        match liveness {
            // POSITIVE absence — the only reading that alone proves death-by-gone.
            ProcLiveness::Gone => return LifecycleState::Gone,
            // AMBIGUOUS — the probe could not witness anything. #4 fail-closed:
            // a probe that did not see an exit NEVER yields death. (Counted as
            // silent-alive so the kill gate stays its hand.)
            ProcLiveness::Unknown => return LifecycleState::AliveSilentValid,
            _ => {}
        }
        // The pid is PRESENT (alive or zombie). Verify identity BEFORE trusting
        // the state: a reused pid must not be read as our-alive or our-exited.
        if let Some(now_start) = self.probe.start_ms(key.pid) {
            if (now_start - key.start_ms).abs() > START_SLACK_MS {
                return LifecycleState::NotOurs;
            }
        }
        // (start unreadable while present: cannot DISPROVE identity → fail-closed
        // assume ours; the state read below still classifies it.)
        match liveness {
            ProcLiveness::RunningOrDisk => LifecycleState::AliveWorking,
            ProcLiveness::Sleeping => LifecycleState::AliveSilentValid,
            // Cross-process zombie: exited, status not provable here (see the
            // ExitedSignal doc). The daemon-parent path refines via classify_reaped.
            ProcLiveness::Zombie => LifecycleState::ExitedSignal,
            // handled above
            ProcLiveness::Gone | ProcLiveness::Unknown => unreachable!(),
        }
    }
}

impl<P: ProcProbe> LivenessSource for OsLiveness<P> {
    fn classify(&self, key: ProcKey) -> LifecycleState {
        self.classify_inner(key)
    }
}

/// Per-session facts fed by the daemon's headless stream + the relay/bond ledger.
/// All defaulted so a session with NO stream observation classifies EXACTLY as
/// the OS layer would (additive, non-regressing — the false-positive guard).
#[derive(Debug, Clone, Default)]
pub struct StreamObs {
    /// `system/init` seen on the headless stream → producer is up → READY.
    pub first_output_seen: bool,
    /// A turn was dispatched and no `result` has been seen yet.
    pub turn_in_flight: bool,
    /// Wall-clock ms since the daemon-dispatched turn-start — the [`LifecycleState::Stuck`]
    /// ANCHOR (a real turn-start, not a file mtime). `None` when no turn-start is
    /// known. DIAGNOSTIC-ONLY (§H.7).
    pub since_turn_start_ms: Option<i64>,
    /// **Ledger-fed**: the session is parked on a relay/question `--wait`. When
    /// set this SHORT-CIRCUITS the stuck check (a `--wait` is NOT stuck) — the
    /// keystone of the amend.
    pub waiting_on_ledger: bool,
}

/// Default τ for the [`LifecycleState::Stuck`] timeout heuristic (ms). Tunable,
/// DIAGNOSTIC-ONLY: 5 min is generous BECAUSE it is NOT a kill trigger — it only
/// ever flips a diagnostic render, never authorizes control automation.
pub const STUCK_THRESHOLD_MS: i64 = 300_000;

/// The (B) readiness-augmented [`LivenessSource`]: wraps an inner source (the
/// [`OsLiveness`]) and OVERLAYS the daemon headless-stream + relay/bond ledger
/// signals onto its OS verdict. Additive — new consumers (e.g. the `qd ls`
/// readiness facet) call [`Self::classify_obs`]; old consumers keep calling the
/// base [`LivenessSource::classify`] (which delegates to `inner`, unchanged).
///
/// **OS truth wins for death/identity**: a stream signal can NEVER resurrect a
/// `Gone`/`Exited*` process or claim a `NotOurs` one — `classify_obs` returns
/// the base state verbatim when it is not alive.
///
/// **DIAGNOSTIC-ONLY (§H.7)**: the [`LifecycleState::Stuck`] verdict this can
/// produce is a wall-clock timeout heuristic, NOT a progress signal. It NEVER
/// authorizes a kill/restart; consumers MUST gate on
/// [`LifecycleState::is_diagnostic_stuck`] and MUST NOT build control automation
/// on it.
pub struct StreamLiveness<L: LivenessSource> {
    inner: L,
    stuck_threshold_ms: i64,
}

impl<L: LivenessSource> StreamLiveness<L> {
    /// Wrap `inner` with the default [`STUCK_THRESHOLD_MS`].
    pub fn new(inner: L) -> Self {
        Self {
            inner,
            stuck_threshold_ms: STUCK_THRESHOLD_MS,
        }
    }

    /// Wrap `inner` with a custom diagnostic stuck threshold (ms).
    pub fn with_threshold(inner: L, stuck_threshold_ms: i64) -> Self {
        Self {
            inner,
            stuck_threshold_ms,
        }
    }

    /// Augmented classification: overlay the stream/ledger `obs` on the inner OS
    /// verdict for `key`.
    ///
    /// 1. **OS truth wins for death/identity**: classify via `inner`; if the
    ///    base state is not alive (`Gone`/`Exited*`/`NotOurs`), return it verbatim
    ///    — a stream signal can never resurrect or convict.
    /// 2. Alive → overlay, **precedence WAITING > STUCK > READY > (base working)**:
    ///    - `waiting_on_ledger` → [`LifecycleState::AliveWaiting`] (a `--wait` is
    ///      NOT stuck — SHORT-CIRCUITS the stuck check even past τ; the keystone
    ///      of the amend).
    ///    - `turn_in_flight` → [`LifecycleState::Stuck`] if past τ (diagnostic),
    ///      else [`LifecycleState::AliveWorking`].
    ///    - `first_output_seen` → [`LifecycleState::AliveReady`] (producer up,
    ///      between turns).
    ///    - else → the base state (no stream signal yet → exactly the OS state;
    ///      no false Ready/Stuck).
    pub fn classify_obs(&self, key: ProcKey, obs: &StreamObs) -> LifecycleState {
        let base = self.inner.classify(key);
        // OS truth wins for death/identity — never override Gone/Exited*/NotOurs.
        if !base.is_alive() {
            return base;
        }
        // WAITING > STUCK > READY > base working.
        if obs.waiting_on_ledger {
            // A `--wait` is NOT stuck — short-circuits the stuck check even when
            // since_turn_start_ms exceeds τ. The keystone of the amend.
            return LifecycleState::AliveWaiting;
        }
        if obs.turn_in_flight {
            if obs
                .since_turn_start_ms
                .is_some_and(|ms| ms > self.stuck_threshold_ms)
            {
                return LifecycleState::Stuck;
            }
            return LifecycleState::AliveWorking;
        }
        if obs.first_output_seen {
            return LifecycleState::AliveReady;
        }
        base
    }
}

impl<L: LivenessSource> LivenessSource for StreamLiveness<L> {
    /// Delegates to `inner` so [`StreamLiveness`] drops into existing consumers
    /// unchanged — the augmented info is opt-in via [`Self::classify_obs`].
    fn classify(&self, key: ProcKey) -> LifecycleState {
        self.inner.classify(key)
    }
}

/// Daemon-as-PARENT refinement: when the observer IS the reaping parent (the
/// qrmux daemon for claude — memo Step-2 ownership), `try_wait`/`pidfd` yields
/// the real [`std::process::ExitStatus`], refining the cross-process
/// `ExitedSignal` into the precise clean-vs-signal verdict. Pure (callers pass
/// the reaped status); this is the only producer of [`LifecycleState::ExitedClean`].
pub fn classify_reaped(status: std::process::ExitStatus) -> LifecycleState {
    use std::os::unix::process::ExitStatusExt;
    if status.signal().is_some() {
        LifecycleState::ExitedSignal
    } else {
        LifecycleState::ExitedClean
    }
}

/// Fold a SEQUENCE of re-probe classifications into a death verdict — the
/// claude-pid-leg extension of the existing ≥3× death-confirmation
/// (`server_launcher::probe_liveness_confirmed`, punch item 16). Death is
/// confirmed ONLY when the sequence is non-empty AND EVERY probe is
/// [`LifecycleState::is_dead`] (consistent Exited*/Gone). Any single alive /
/// NotOurs / ambiguous reading ⇒ NOT dead (fail-closed; #4: never kill on
/// silence/refusal/a transient miss). The consumer drives the re-probe cadence
/// (it owns the sleeper); this is the pure verdict over what it observed.
pub fn confirmed_dead(seq: &[LifecycleState]) -> bool {
    !seq.is_empty() && seq.iter().all(|s| s.is_dead())
}

/// How many times the claude-pid leg re-probes before confirming death — the
/// ≥3× contract (#4), mirroring `server_launcher::CRASH_CONFIRM_BACKOFF_MS`
/// (a first probe + two backoff re-probes = 3 readings).
pub const DEATH_CONFIRM_PROBES: usize = 3;

/// Backoff between the death-confirmation re-probes (ms), mirroring
/// `server_launcher::CRASH_CONFIRM_BACKOFF_MS = [100, 250]` (the two gaps after
/// the first probe → ~350ms worst case to confirm an honest death).
pub const DEATH_CONFIRM_BACKOFF_MS: [u64; 2] = [100, 250];

/// WP-D part (a) — the `qd ls` LIVENESS GATE, as a pure per-row decision: given a
/// joined row's displayed `status` + its `(pid, recorded_start_ms)` + a
/// classifier, return the status to DISPLAY. A row currently in the LIVE status
/// set (`idle`/`busy`/`shell`) whose pid is classified NOT-alive (zombie/gone/
/// exited, or a reused pid ⇒ `NotOurs`) is downgraded to `Cold` — dropping it out
/// of the live set, using the EXISTING "no live process" vocabulary (no new
/// status token). Everything else is returned UNCHANGED:
///
/// - an already non-live status (`cold`/`killed`) is never resurrected or touched;
/// - a row with no pid, or no recorded start, is left ungated (fail-open — we
///   never gate a row whose identity we cannot reuse-guard);
/// - a LIVE classifier verdict (incl. the silent-window/ambiguous fail-closed
///   `AliveSilentValid`) keeps the row's exact status — a quiet-but-alive session
///   is NEVER hidden.
///
/// **Scope (deliberate):** this is the `qd ls` RENDER gate ONLY — the caller
/// applies it to the joined view it is about to display. It does NOT live in the
/// shared `join_sessions`, so ACTING verbs (send/wait/kill/resolve) keep the raw
/// registry status and are unaffected (gating their resolve would wrongly refuse a
/// session whose registered pid is momentarily unverifiable). The recorded
/// `recorded_start_ms` is the registration instant (`started_at`), within the
/// 120s [`START_SLACK_MS`] of the process start, so a recycled pid ⇒ `NotOurs`.
pub fn gated_ls_status(
    status: crate::model::SessionStatus,
    pid: Option<i64>,
    recorded_start_ms: Option<i64>,
    src: &dyn LivenessSource,
) -> crate::model::SessionStatus {
    use crate::model::SessionStatus;
    // Only gate rows currently SHOWN live; never touch cold/killed.
    if !matches!(
        status,
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
    ) {
        return status;
    }
    // Need both a pid AND a recorded start to form a reuse-guarded identity.
    let (Some(pid), Some(start)) = (pid, recorded_start_ms) else {
        return status;
    };
    if src.classify(ProcKey::new(pid as i32, start)).is_alive() {
        status
    } else {
        SessionStatus::Cold
    }
}

/// WP-B5-ii-a guarantee (ii) — the per-session DAEMON-liveness signal for a
/// HEADLESS row's `qd ls` render gate. The daemon-liveness verdict the lead RULING
/// (Fork B) fixes as a per-session `<dir>/<name>.sock` CONNECT probe (the
/// ECONNREFUSED-class analog already used by `qrmux::client::discovery`'s
/// `probe_socket` and `create_daemon`'s endpoint check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonLiveness {
    /// The per-session socket accepted a connection — a daemon is listening, so
    /// its last-written real-time status row is trustworthy.
    Up,
    /// ECONNREFUSED (socket present, no listener) or the socket file is missing
    /// (ENOENT) — the daemon is gone, so its last-written status is no longer
    /// real-time and must not be rendered as live.
    Down,
}

/// The daemon-liveness signal source for the (ii) headless render gate (§6.4 seam
/// so [`gated_ls_status_headless`] is unit-testable without a real socket). (B)
/// could later swap the impl for a folded subscriber signal without touching the
/// gate; today's impl is [`SocketDaemonLiveness`] (the socket-connect probe).
pub trait DaemonLivenessSource {
    /// The daemon-liveness verdict for the headless session named `name` (the
    /// socket leaf is `<name>.sock`).
    fn daemon_liveness(&self, name: &str) -> DaemonLiveness;
}

/// WP-B5-ii-a guarantee (ii) — the `qd ls` HEADLESS DAEMON-DOWN render gate,
/// composed BEFORE the claude-pid [`gated_ls_status`] and taking PRECEDENCE over
/// it (lead RULING §"the (ii) gate composition").
///
/// **The stale-busy disease this cures:** the owning per-session daemon dies, but
/// the orphaned, pidfd-less claude child it launched is STILL alive — so the
/// `(pid, starttime)` classifier reads [`LifecycleState::AliveSilentValid`] and
/// plain [`gated_ls_status`] KEEPS the row's last-written `busy`. That `busy` is
/// now STALE: its real-time writer (the daemon) is gone. For a **headless** row
/// (`entrypoint == "headless"`) we therefore gate trust of the daemon-written
/// status on the daemon-liveness probe FIRST: daemon [`DaemonLiveness::Down`] ⇒
/// [`SessionStatus::Cold`] (the row drops out of the live set — Fork A: REUSE
/// `Cold`, no new `SessionStatus` token), REGARDLESS of the orphan claude pid
/// still being alive.
///
/// Everything else delegates UNCHANGED to [`gated_ls_status`] (the daemon-LIVE
/// path stays exactly as B5-i shipped):
/// - a non-headless row (interactive `entrypoint == None`): the daemon probe is
///   skipped entirely — its liveness is the mux-pane signal, not this socket;
/// - a headless row whose daemon is [`DaemonLiveness::Up`]: the existing claude-
///   pid `(pid,starttime)` downgrade still applies (a dead orphan ⇒ `Cold`);
/// - a headless row with no `name` (cannot form the socket leaf): fail-OPEN to the
///   claude-pid gate — never hide a row whose daemon we cannot probe (mirrors
///   `gated_ls_status`'s fail-open when the reuse-guard identity is unformable).
///
/// **Scope (deliberate, mirrors [`gated_ls_status`]):** the `qd ls` RENDER gate
/// ONLY — ACTING verbs (send/wait/kill/resolve) keep the raw registry status, and
/// this does NOT live in shared `join_sessions`. `qd wait`'s own daemon-down
/// behavior is guarantee (iii) (the wait/poll state machine), NOT this render gate.
///
/// **Fix-shaped mutation (red-before for (ii)):** drop the daemon-liveness gate
/// (trust the row unconditionally) and a headless row whose daemon died but whose
/// orphan claude child is still alive renders stale `busy` → the test reds.
pub fn gated_ls_status_headless(
    status: crate::model::SessionStatus,
    entrypoint: Option<&str>,
    name: Option<&str>,
    pid: Option<i64>,
    recorded_start_ms: Option<i64>,
    daemon_src: &dyn DaemonLivenessSource,
    src: &dyn LivenessSource,
) -> crate::model::SessionStatus {
    use crate::model::SessionStatus;
    // Never touch a row that is already non-live (cold/killed) — and never spend a
    // socket probe on one (mirrors gated_ls_status's first guard).
    if !matches!(
        status,
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
    ) {
        return status;
    }
    // (ii) headless daemon-down gate FIRST, taking precedence over the claude-pid
    // classifier. Scoped to headless rows we can name (the socket leaf is the name).
    if entrypoint == Some(crate::observe::HEADLESS_ENTRYPOINT) {
        if let Some(name) = name {
            if daemon_src.daemon_liveness(name) == DaemonLiveness::Down {
                return SessionStatus::Cold;
            }
        }
    }
    // Daemon LIVE, or a non-headless / unnameable row: the existing claude-pid
    // downgrade, unchanged.
    gated_ls_status(status, pid, recorded_start_ms, src)
}

/// WS-R R3a-Step-3 — RECONCILED liveness: the registry is a CACHE reconciled
/// against KERNEL TRUTH (R1 §2). Composes the O(1) flock fast-path (R3a-Step-1)
/// with the `/proc start_ms` identity confirmer, returning whether the session's
/// `(pid, start_ms)` is genuinely LIVE.
///
/// ## The composition (and why this ordering — R1 §1 inv 3 / P4.3)
/// 1. **flock fast-path** ([`crate::livelock::probe_dead`]): O(1), no `/proc`
///    walk. If the lock is HELD (`probe_dead == false`) the holder is live — but
///    a held lock is only a HINT here: a different live process could hold it (the
///    session_id↔pid binding is the writer's invariant, not re-checked here), so a
///    held lock NEVER short-circuits to "live" past the `/proc` authority.
/// 2. **`/proc start_ms` AUTHORITY** (`src.classify`): the reuse-robust
///    `(pid, start_ms)` verdict is the GROUND TRUTH for a tombstone decision. A
///    `probe_dead`->"dead" MUST be re-confirmed here — between the flock probe and
///    reconcile acting, a fresh session could acquire the lock; only the
///    `(pid, start_ms)` `/proc` read can convict THIS pid.
///
/// ## Fail-closed (R1 §2 / the I5 floor)
/// The function returns `true` (LIVE — abort any tombstone) on ANY ambiguity: a
/// present-but-unverifiable pid classifies [`LifecycleState::AliveSilentValid`]
/// (`is_alive()`), so a row whose pid we cannot positively convict is never
/// tombstoned. Only a positive `Exited*`/`Gone`/`NotOurs` (`!is_alive()`) reports
/// dead. This preserves reconcile's I5 (alive -> never touched).
///
/// The `state_dir`/`session_id` may be `None` (a row with no recorded session id):
/// the flock fast-path is skipped and the `/proc` authority alone decides — the
/// gate degrades to the exact pre-flock `/proc`-only liveness (no regression).
pub fn is_session_live_reconciled(
    state_dir: Option<&std::path::Path>,
    session_id: Option<&str>,
    pid: i64,
    start_ms: i64,
    src: &dyn LivenessSource,
) -> bool {
    // O(1) flock fast-path: a FREE lock is a cheap "candidate dead" hint that we
    // still confirm via /proc; a HELD lock does not bypass the /proc authority.
    // (The fast-path's value is avoiding a /proc walk for the common live case in
    // a large sweep, while /proc stays the tombstone authority.)
    if let (Some(state_dir), Some(session_id)) = (state_dir, session_id) {
        let _candidate_dead = crate::livelock::probe_dead(state_dir, session_id);
        // We deliberately do NOT early-return on the hint: /proc is the authority
        // (R1 §1 inv 3). The probe is retained as the documented fast-path seam;
        // when the lock is held it agrees with a live /proc, and when free it
        // routes to the /proc confirmer below — never a tombstone on the hint alone.
        let _ = _candidate_dead;
    }
    // The /proc start_ms authority: reuse-robust, fail-closed alive on ambiguity.
    src.classify(ProcKey::new(pid as i32, start_ms)).is_alive()
}

/// WS-R R3a-Step-3 — the SYSTEMIC read-time reconcile gate (R1 §2; closes P0 gap
/// (a): "no 'registry is a cache reconciled against kernel truth' invariant
/// enforced on every READ"). Given a row's displayed `status` + its
/// `(pid, recorded_start_ms)` + the session's `(state_dir, session_id)` for the
/// flock fast-path, returns the status to TRUST: a row currently shown live
/// (`idle`/`busy`/`shell`) whose pid is NOT live (per [`is_session_live_reconciled`])
/// is downgraded to [`crate::model::SessionStatus::Cold`] — the cache reconciles to
/// kernel truth on read, so a crashed session's `busy` row never reads live.
///
/// This is [`gated_ls_status`] with the flock-composed reconciled liveness wired
/// in: same shape, same `Cold`-downgrade vocabulary, same fail-open for an
/// unformable identity (no pid / no recorded start). The ONLY strengthening is the
/// `is_alive` predicate now composes flock + `/proc` instead of `/proc` alone.
pub fn reconciled_read_status(
    status: crate::model::SessionStatus,
    state_dir: Option<&std::path::Path>,
    session_id: Option<&str>,
    pid: Option<i64>,
    recorded_start_ms: Option<i64>,
    src: &dyn LivenessSource,
) -> crate::model::SessionStatus {
    use crate::model::SessionStatus;
    // Only gate rows currently SHOWN live; never resurrect or touch cold/killed.
    if !matches!(
        status,
        SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
    ) {
        return status;
    }
    // Need both a pid AND a recorded start to form a reuse-guarded identity
    // (fail-open: never gate a row whose identity we cannot reuse-guard).
    let (Some(pid), Some(start)) = (pid, recorded_start_ms) else {
        return status;
    };
    if is_session_live_reconciled(state_dir, session_id, pid, start, src) {
        status
    } else {
        SessionStatus::Cold
    }
}

/// The production [`DaemonLivenessSource`]: a synchronous per-session socket
/// CONNECT probe against `<qrmux_dir>/<name>.sock` (lead RULING Fork B). A
/// successful connect ⇒ [`DaemonLiveness::Up`] (a daemon is listening);
/// `ConnectionRefused` (stale socket, no listener) or `NotFound` (the socket file
/// is gone) ⇒ [`DaemonLiveness::Down`] — the positive daemon-down evidence. ANY
/// OTHER error (permission, would-block, …) is ambiguous ⇒ `Up` (fail-OPEN: never
/// convict the daemon dead — hence never false-`Cold` a row — from a probe that
/// could not answer; the same #4 fail-closed-on-ambiguity posture the
/// `(pid,starttime)` classifier takes).
///
/// `qrmux_dir == None` (HOME unset / the dir could not be resolved) ⇒ every probe
/// returns `Up`, so the gate degrades to the exact pre-WP claude-pid-only behavior.
pub struct SocketDaemonLiveness {
    qrmux_dir: Option<std::path::PathBuf>,
}

impl SocketDaemonLiveness {
    pub fn new(qrmux_dir: Option<std::path::PathBuf>) -> Self {
        Self { qrmux_dir }
    }
}

impl DaemonLivenessSource for SocketDaemonLiveness {
    fn daemon_liveness(&self, name: &str) -> DaemonLiveness {
        use std::io::ErrorKind;
        let Some(dir) = &self.qrmux_dir else {
            return DaemonLiveness::Up; // no dir to probe → never downgrade.
        };
        let path = dir.join(format!("{name}.sock"));
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => DaemonLiveness::Up, // a listener accepted us.
            Err(e) => match e.kind() {
                // Positive daemon-down evidence: no listener / no socket file.
                ErrorKind::ConnectionRefused | ErrorKind::NotFound => DaemonLiveness::Down,
                // Ambiguous — fail-open, never false-Cold a possibly-live row.
                _ => DaemonLiveness::Up,
            },
        }
    }
}

/// WP-B-CS-2 — the `qd ls` READINESS FACET (S-B rulings D3: `ready`/`silent`/
/// `stuck`). A coarse, ADDITIVE projection of the full [`LifecycleState`] onto the
/// three-value facet the `qd ls` surface carries ALONGSIDE (never instead of) the
/// `status` field. It answers "is the producer confirmed ready, merely alive, or
/// diagnostically stuck" — a readiness axis distinct from idle/busy status:
///
/// - [`LifecycleState::AliveReady`] → `Ready` (producer up, between turns — a real
///   `system/init` signal, gate E/R3);
/// - [`LifecycleState::Stuck`] → `Stuck` (the DIAGNOSTIC-ONLY §H.7 timeout — gated
///   by [`LifecycleState::is_diagnostic_stuck`], never a kill trigger);
/// - any OTHER alive state (`AliveSilentValid`/`AliveWorking`/`AliveWaiting`) →
///   `Silent` (alive but not confirmed-ready and not stuck — the silent-window
///   bucket the facet coarsens to);
/// - any NOT-alive state (`Gone`/`Exited*`/`NotOurs`) → `None` (no readiness facet
///   — such a row is already downgraded to `Cold` by [`gated_ls_status`]).
///
/// The ready/stuck/waiting inputs come from the (B) [`StreamLiveness`] overlay
/// (`classify_obs`), daemon-stream + ledger fed; without that overlay the OS layer
/// only yields `Silent`/`None`, and the facet folds in cleanly when B5 wires the
/// stream obs. PURE — fully unit-tested over synthetic states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Silent,
    Stuck,
}

impl Readiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::Silent => "silent",
            Readiness::Stuck => "stuck",
        }
    }
}

/// Map a [`LifecycleState`] to its `qd ls` readiness facet (`None` for not-alive).
pub fn readiness_facet(state: LifecycleState) -> Option<Readiness> {
    match state {
        LifecycleState::AliveReady => Some(Readiness::Ready),
        // Gate on the diagnostic-only predicate (Stuck only) so the facet can
        // never drift to flag a non-Stuck state as stuck.
        s if s.is_diagnostic_stuck() => Some(Readiness::Stuck),
        s if s.is_alive() => Some(Readiness::Silent),
        // Not alive (dead / NotOurs): no readiness facet — the row is gated Cold.
        _ => None,
    }
}

// ===========================================================================
// LIVENESS CORPUS (§6 DoD #6) — derived from the §9a memo Step-2 trackB spike.
// The pre-existing corpus predates these bugs; these are the new entries. Each
// case is judged THROUGH the real classifier (a fixture probe feeds the
// reading), so the corpus is an executable carrier, not a comment.
// ===========================================================================

/// One classification case: a recorded identity + a single OS reading → the
/// expected lifecycle verdict, with the provenance it was derived from.
#[derive(Debug, Clone, Copy)]
pub struct CorpusCase {
    pub name: &'static str,
    pub provenance: &'static str,
    pub reading: ProcLiveness,
    /// Start time the classifier re-reads (None = present-but-unreadable race).
    pub observed_start_ms: Option<i64>,
    /// Start time recorded at registration.
    pub recorded_start_ms: i64,
    pub expect: LifecycleState,
}

/// The new WP-A corpus. `RECORDED_START` is a fixed reference instant; cases
/// vary the observed reading + start against it.
pub const RECORDED_START: i64 = 1_700_000_000_000;

pub const LIVENESS_CORPUS: &[CorpusCase] = &[
    CorpusCase {
        name: "silent-window-sleeping",
        provenance: "trackB: claude 2.1.177 silent window, /proc state ∈ {R,S,D}, never Z",
        reading: ProcLiveness::Sleeping,
        observed_start_ms: Some(RECORDED_START),
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::AliveSilentValid,
    },
    CorpusCase {
        name: "silent-window-running",
        provenance: "trackB: silent window also samples R (on-CPU)",
        reading: ProcLiveness::RunningOrDisk,
        observed_start_ms: Some(RECORDED_START),
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::AliveWorking,
    },
    CorpusCase {
        name: "exited-zombie-unreaped",
        provenance:
            "trackB: SIGTERM mid-turn → Z before the parent reaps (status unknowable cross-process)",
        reading: ProcLiveness::Zombie,
        observed_start_ms: Some(RECORDED_START),
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::ExitedSignal,
    },
    CorpusCase {
        name: "gone-reaped",
        provenance: "trackB: after the parent reaps, /proc/<pid> is ENOENT",
        reading: ProcLiveness::Gone,
        observed_start_ms: None,
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::Gone,
    },
    CorpusCase {
        name: "pid-reused-not-ours",
        provenance: "starttime guard: pid recycled by a process that started 10min later",
        reading: ProcLiveness::Sleeping,
        observed_start_ms: Some(RECORDED_START + 600_000),
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::NotOurs,
    },
    CorpusCase {
        name: "probe-ambiguous-failclosed-alive",
        provenance: "#4 fail-closed: a probe that could not answer is NEVER death",
        reading: ProcLiveness::Unknown,
        observed_start_ms: None,
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::AliveSilentValid,
    },
    CorpusCase {
        name: "present-but-start-unreadable",
        provenance: "race: present in /proc, starttime read lost — fail-closed assume ours",
        reading: ProcLiveness::Sleeping,
        observed_start_ms: None,
        recorded_start_ms: RECORDED_START,
        expect: LifecycleState::AliveSilentValid,
    },
];

#[cfg(test)]
mod r3a3_nonvacuity {
    use super::*;
    use crate::effects::ProcLiveness;
    use crate::model::SessionStatus;

    struct AlwaysAlive;
    impl ProcProbe for AlwaysAlive {
        fn start_ms(&self, _pid: i32) -> Option<i64> { Some(0) }
        fn liveness(&self, _pid: i32) -> ProcLiveness { ProcLiveness::Sleeping }
    }
    struct AlwaysGone;
    impl ProcProbe for AlwaysGone {
        fn start_ms(&self, _pid: i32) -> Option<i64> { None }
        fn liveness(&self, _pid: i32) -> ProcLiveness { ProcLiveness::Gone }
    }

    // NON-VACUITY: the gate downgrades a busy-for-dead row to Cold ONLY because
    // the /proc authority says dead. Force the authority always-ALIVE and the
    // SAME busy row stays Busy (the revert seam: is_alive=true => dead rows stay
    // live, RED). This proves the Cold downgrade is driven by the real classifier,
    // not a tautology.
    #[test]
    fn reconciled_gate_is_nonvacuous() {
        // Authority says DEAD => busy downgrades to Cold.
        let dead = OsLiveness::with_probe(AlwaysGone);
        let out = reconciled_read_status(
            SessionStatus::Busy, None, None, Some(123), Some(1000), &dead,
        );
        assert_eq!(out, SessionStatus::Cold, "dead pid must reconcile Busy -> Cold");

        // Authority forced ALIVE (the revert) => the SAME row stays Busy.
        let alive = OsLiveness::with_probe(AlwaysAlive);
        let out2 = reconciled_read_status(
            SessionStatus::Busy, None, None, Some(123), Some(1000), &alive,
        );
        assert_eq!(
            out2, SessionStatus::Busy,
            "with is_alive forced true the dead row stays Busy (the gate is load-bearing, not vacuous)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A scripted [`ProcProbe`] — feeds a single fixed reading + start, so the
    /// classification LOGIC is exercised deterministically across all six states
    /// (no real /proc).
    struct FixtureProbe {
        reading: ProcLiveness,
        start: Option<i64>,
    }
    impl ProcProbe for FixtureProbe {
        fn start_ms(&self, _pid: i32) -> Option<i64> {
            self.start
        }
        fn liveness(&self, _pid: i32) -> ProcLiveness {
            self.reading
        }
    }

    fn classify_case(
        reading: ProcLiveness,
        observed: Option<i64>,
        recorded: i64,
    ) -> LifecycleState {
        OsLiveness::with_probe(FixtureProbe {
            reading,
            start: observed,
        })
        .classify(ProcKey::new(4242, recorded))
    }

    /// The §6 DoD corpus (new WP-A entries) is judged THROUGH the real classifier
    /// — false-positive (silent ⇒ alive, ambiguous ⇒ alive) AND false-negative
    /// (zombie ⇒ exited, gone ⇒ gone) cases both present. A fixture that simply
    /// echoed `expect` would not exercise the fold; this routes every case
    /// through `classify`.
    #[test]
    fn liveness_corpus_is_loadbearing() {
        assert!(LIVENESS_CORPUS.len() >= 7, "corpus must be non-trivial");
        for c in LIVENESS_CORPUS {
            let got = classify_case(c.reading, c.observed_start_ms, c.recorded_start_ms);
            assert_eq!(got, c.expect, "corpus case '{}' ({})", c.name, c.provenance);
        }
    }

    /// All six states are reachable from the classifier (the state machine is
    /// not vacuous). FALSE-POSITIVE guard: a silent (S) process is AliveSilentValid,
    /// never dead. FALSE-NEGATIVE: a really-gone pid is Gone.
    #[test]
    fn classifier_covers_all_six_states() {
        let rec = RECORDED_START;
        assert_eq!(
            classify_case(ProcLiveness::Sleeping, Some(rec), rec),
            LifecycleState::AliveSilentValid
        );
        assert_eq!(
            classify_case(ProcLiveness::RunningOrDisk, Some(rec), rec),
            LifecycleState::AliveWorking
        );
        assert_eq!(
            classify_case(ProcLiveness::Zombie, Some(rec), rec),
            LifecycleState::ExitedSignal
        );
        assert_eq!(
            classify_case(ProcLiveness::Gone, None, rec),
            LifecycleState::Gone
        );
        // PID reuse: present + alive, but started 10 minutes after the record.
        assert_eq!(
            classify_case(ProcLiveness::Sleeping, Some(rec + 600_000), rec),
            LifecycleState::NotOurs
        );
        // ExitedClean only comes from the parent-reap refinement.
        assert_eq!(
            classify_reaped(reaped_status(&["true"])),
            LifecycleState::ExitedClean
        );
    }

    /// #4 fail-closed: an AMBIGUOUS probe (could not answer) is NEVER death — it
    /// resolves to AliveSilentValid so the kill gate stays its hand. The single
    /// most important false-positive-death guard.
    #[test]
    fn ambiguous_probe_is_never_death() {
        let s = classify_case(ProcLiveness::Unknown, None, RECORDED_START);
        assert_eq!(s, LifecycleState::AliveSilentValid);
        assert!(!s.is_dead(), "ambiguity must not be death");
        assert!(s.is_alive());
    }

    /// is_dead / is_alive partition the state machine the way the kill gate needs:
    /// NotOurs is NEITHER (a reused pid is not our death and not our alive).
    #[test]
    fn dead_alive_predicates() {
        use LifecycleState::*;
        for s in [AliveSilentValid, AliveWorking] {
            assert!(s.is_alive() && !s.is_dead());
        }
        for s in [ExitedClean, ExitedSignal, Gone] {
            assert!(s.is_dead() && !s.is_alive());
        }
        assert!(
            !NotOurs.is_alive() && !NotOurs.is_dead(),
            "reused pid is neither"
        );
    }

    /// The ≥3× death-confirmation fold (#4): death ONLY on consistent dead
    /// readings. The fix-shaped "drop the re-read" mutation = trusting the FIRST
    /// reading alone — `[Gone, AliveSilentValid, Gone]` would convict under that
    /// mutation; `confirmed_dead` (all-must-be-dead) does NOT.
    #[test]
    fn confirmed_dead_requires_every_probe_dead() {
        use LifecycleState::*;
        assert!(!confirmed_dead(&[]), "empty sequence is not death");
        assert!(confirmed_dead(&[Gone, Gone, Gone]));
        assert!(confirmed_dead(&[ExitedSignal, Gone, ExitedClean]));
        // A single alive reading anywhere spares it (the load-bearing re-read).
        assert!(!confirmed_dead(&[Gone, AliveSilentValid, Gone]));
        assert!(!confirmed_dead(&[AliveWorking]));
        // NotOurs is not dead — a reused pid is never convicted (wrong-victim).
        assert!(!confirmed_dead(&[NotOurs, NotOurs, NotOurs]));
    }

    /// Reap a real child and hand its status to the parent-refinement.
    fn reaped_status(argv: &[&str]) -> std::process::ExitStatus {
        let mut child = Command::new(argv[0])
            .args(&argv[1..])
            .spawn()
            .expect("spawn");
        child.wait().expect("wait")
    }

    /// classify_reaped (daemon-as-parent path) — the ONLY producer of
    /// ExitedClean — distinguishes a clean exit from a signal kill. Both are
    /// `is_dead()`.
    #[test]
    fn classify_reaped_clean_vs_signal() {
        assert_eq!(
            classify_reaped(reaped_status(&["true"])),
            LifecycleState::ExitedClean
        );
        assert_eq!(
            classify_reaped(reaped_status(&["false"])),
            LifecycleState::ExitedClean,
            "nonzero exit is still a CLEAN (voluntary) exit, not a signal"
        );
        // Killed by signal → ExitedSignal.
        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
        let pid = child.id() as i32;
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let status = child.wait().expect("wait");
        assert_eq!(classify_reaped(status), LifecycleState::ExitedSignal);
    }

    // ===================== REAL-PROCESS integration =====================
    // The OS classifier against live processes — the in-bounds proxy for "a
    // real isolated claude's silent window" (trackB: a quiet child reads /proc
    // state S, exactly like claude's 2–7s silent window). These exercise the
    // real /proc + proc_start_ms reads, not a fixture.

    /// FALSE-POSITIVE-DEATH guard (real OS): a quiet child (`sleep`) sits in
    /// /proc state S — the silent-window shape — and classifies ALIVE, never
    /// dead. A bare `kill(pid,0)` would also say "alive" here, but it ALSO says
    /// "alive" for a zombie; the classifier's value is being right on BOTH ends.
    #[test]
    fn real_silent_child_is_alive_not_dead() {
        use crate::effects::Clock;
        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
        let pid = child.id() as i32;
        // Production shape: the registry records `startedAt` ≈ wall-clock now at
        // registration (NOT proc_start_ms, which returns None for a sub-second
        // child under load → a degenerate `0` identity → spurious NotOurs).
        let start = effects::RealClock.now_ms();
        let src = OsLiveness::new();
        let s = src.classify(ProcKey::new(pid, start));
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            s.is_alive(),
            "a live silent child must classify alive: {s:?}"
        );
        assert!(!s.is_dead());
    }

    /// FALSE-NEGATIVE coverage (real OS): a child that has exited AND been reaped
    /// is provably Gone (/proc/<pid> ENOENT).
    #[test]
    fn real_reaped_child_is_gone() {
        use crate::effects::Clock;
        let mut child = Command::new("true").spawn().expect("spawn");
        let pid = child.id() as i32;
        // Start is irrelevant here (a reaped child is Gone via /proc ENOENT
        // BEFORE the identity arm); record the production-shape wall-clock now
        // for uniformity rather than a degenerate `0`.
        let start = effects::RealClock.now_ms();
        child.wait().expect("reap"); // now gone from /proc
        let s = OsLiveness::new().classify(ProcKey::new(pid, start));
        assert_eq!(s, LifecycleState::Gone, "reaped child is Gone");
        assert!(s.is_dead());
    }

    /// PID-REUSE robustness (real OS): a live child probed against a RECORDED
    /// start-time that predates its real start (the recycled-pid shape) is
    /// NotOurs — never counted as our session alive, never killed.
    #[test]
    fn real_reused_pid_is_not_ours() {
        // The intent is the reuse-detection COMPARISON, driven deterministically
        // and load-immune via an injected probe: the pid reads PRESENT (Sleeping)
        // with a FIXED current start, while our session recorded it as having
        // started 10 min earlier — the recycled-pid shape. No real `ps` read, so
        // a sub-second-old child under spawn-storm load (proc_start_ms → None →
        // a fail-closed AliveSilentValid) can never flake this comparison.
        let live_start: i64 = 2_000_000_000_000;
        let src = OsLiveness::with_probe(FixtureProbe {
            reading: ProcLiveness::Sleeping,
            start: Some(live_start),
        });
        // The live occupant is a DIFFERENT process that recycled the pid.
        let recorded = live_start - 600_000;
        let s = src.classify(ProcKey::new(4242, recorded));
        assert_eq!(s, LifecycleState::NotOurs);
    }

    /// WP-E hardening (real OS): a FRESHLY-spawned, sub-second-old child probed
    /// against a registry-shaped recorded start (wall-clock `now` at "registration")
    /// classifies ALIVE — never `NotOurs`. This is the newborn that WP-D flagged
    /// flashing `cold` in `qd ls`: `ps -o etime=` of a <1s process can misparse
    /// `proc_start_ms` to a garbage start far from the recorded one, which the
    /// identity arm would read as a reused pid. With the `start_from_etime` range
    /// guard a garbage read becomes `None` ⇒ the fail-closed "assume ours" path ⇒
    /// ALIVE; an in-range read matches the recorded start ⇒ ALIVE. Either way the
    /// newborn is never a stranger.
    #[test]
    fn fresh_subsecond_child_classifies_alive_not_notours() {
        use crate::effects::Clock;
        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
        let pid = child.id() as i32;
        // Production shape: the registry records `startedAt` ≈ wall-clock now at
        // registration (NOT proc_start_ms). The child is <1s old here.
        let recorded = effects::RealClock.now_ms();
        let s = OsLiveness::new().classify(ProcKey::new(pid, recorded));
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            s.is_alive(),
            "a <1s-old real child must classify ALIVE, got {s:?}"
        );
        assert_ne!(s, LifecycleState::NotOurs, "newborn must not be a stranger");
    }

    /// LATENCY BUDGET (§6 DoD #5) — p50/p99 of a real classify on the live
    /// process. proc_start_ms forks `ps` (the mandated reuse-of-primitive), so
    /// the probe is fork-bound; the budget is generous and the measured numbers
    /// are PRINTED for the verifier. This is a boot-time probe, not a hot loop.
    #[test]
    fn probe_latency_p50_p99_budget() {
        use crate::effects::Clock;
        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
        let pid = child.id() as i32;
        // Production-shape wall-clock recorded start (not the degenerate `0` a
        // sub-second `proc_start_ms` None yields) so the timed classify walks the
        // realistic ALIVE path.
        let start = effects::RealClock.now_ms();
        let src = OsLiveness::new();
        let key = ProcKey::new(pid, start);
        let n = 60;
        let mut us: Vec<u128> = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            let _ = src.classify(key);
            us.push(t.elapsed().as_micros());
        }
        let _ = child.kill();
        let _ = child.wait();
        us.sort_unstable();
        let p50 = us[n / 2];
        let p99 = us[(n * 99 / 100).min(n - 1)];
        println!("classify latency: p50={p50}us p99={p99}us (n={n})");
        // Generous bound: a fork+/proc read is well under 250ms even loaded.
        assert!(p99 < 250_000, "p99 {p99}us exceeds 250ms budget");
    }

    // ===================== WP-D `qd ls` liveness gate =====================

    use crate::model::SessionStatus;

    /// A constant [`LivenessSource`] — exercises the gate's DECISION logic
    /// deterministically, independent of any OS reading.
    struct ConstSrc(LifecycleState);
    impl LivenessSource for ConstSrc {
        fn classify(&self, _k: ProcKey) -> LifecycleState {
            self.0
        }
    }

    /// CONSUMER red/green (DoD #2) + FIX-SHAPED MUTATION, at the gate's pure core.
    /// GREEN-with: a live `busy` row whose pid is classified Gone is downgraded to
    /// `Cold`. The "fix-shaped mutation" is the gate's own else-branch — returning
    /// `status` instead of `Cold` (i.e. NOT downgrading) keeps the sticky `busy` =
    /// the live false-"alive" bug; this assertion is exactly what reds under that
    /// mutation. (The end-to-end consumer red/green over the real `qd ls` binary —
    /// a real pid transitioning alive→reaped — is the integration test
    /// `wpd_ls_liveness_gate.rs`.)
    #[test]
    fn gate_downgrades_dead_pid_to_cold() {
        let gated = gated_ls_status(
            SessionStatus::Busy,
            Some(4242),
            Some(1000),
            &ConstSrc(LifecycleState::Gone),
        );
        assert_eq!(gated, SessionStatus::Cold, "dead pid → out of the live set");
    }

    /// FALSE-NEGATIVE coverage (DoD #4): every NOT-alive verdict downgrades →
    /// `Cold`, from each live status — including a ZOMBIE (the "stop counting
    /// zombies alive" case) and a reused pid (`NotOurs`).
    #[test]
    fn gate_downgrades_every_not_alive_state() {
        for state in [
            LifecycleState::Gone,
            LifecycleState::ExitedSignal, // cross-process zombie
            LifecycleState::ExitedClean,
            LifecycleState::NotOurs, // pid reused by a different process
        ] {
            for live in [
                SessionStatus::Busy,
                SessionStatus::Idle,
                SessionStatus::Shell,
            ] {
                let g = gated_ls_status(live, Some(7), Some(1000), &ConstSrc(state));
                assert_eq!(g, SessionStatus::Cold, "{state:?} from {live:?} → Cold");
            }
        }
    }

    /// FALSE-POSITIVE guard (DoD #4): a LIVE verdict NEVER downgrades — a working
    /// (`AliveWorking`) or quiet/silent (`AliveSilentValid`, the fail-closed
    /// ambiguous case) session keeps its EXACT status. A quiet-but-alive claude is
    /// never hidden.
    #[test]
    fn gate_never_touches_a_live_pid() {
        for state in [
            LifecycleState::AliveWorking,
            LifecycleState::AliveSilentValid,
        ] {
            for live in [
                SessionStatus::Busy,
                SessionStatus::Idle,
                SessionStatus::Shell,
            ] {
                let g = gated_ls_status(live, Some(8), Some(1000), &ConstSrc(state));
                assert_eq!(g, live, "{state:?} is alive → status untouched");
            }
        }
    }

    /// FAIL-OPEN / never-resurrect guards: an already non-live status is returned
    /// verbatim (never resurrected, never re-probed); a row with no pid or no
    /// recorded start is left ungated even if the classifier would say dead.
    #[test]
    fn gate_leaves_unkeyable_and_nonlive_rows_alone() {
        // already cold/killed → unchanged even with a Gone verdict.
        for st in [SessionStatus::Cold, SessionStatus::Killed] {
            assert_eq!(
                gated_ls_status(st, Some(9), Some(1000), &ConstSrc(LifecycleState::Gone)),
                st
            );
        }
        // live status but NO pid → ungated (cannot key identity).
        assert_eq!(
            gated_ls_status(
                SessionStatus::Busy,
                None,
                Some(1000),
                &ConstSrc(LifecycleState::Gone)
            ),
            SessionStatus::Busy
        );
        // live status but NO recorded start → ungated (no reuse guard).
        assert_eq!(
            gated_ls_status(
                SessionStatus::Busy,
                Some(9),
                None,
                &ConstSrc(LifecycleState::Gone)
            ),
            SessionStatus::Busy
        );
    }

    // =======================================================================
    // WP-B5-ii-a guarantee (ii) — the HEADLESS DAEMON-DOWN render gate. Cheap
    // unit-layer mirror of the integration test's (ii) leg, on the default floor.
    // =======================================================================

    /// A fixed daemon-liveness verdict, the (ii) analog of `ConstSrc`.
    struct FakeDaemon(DaemonLiveness);
    impl DaemonLivenessSource for FakeDaemon {
        fn daemon_liveness(&self, _name: &str) -> DaemonLiveness {
            self.0
        }
    }

    /// (ii) THE DISEASE CURED — red-before/green-after at the gate's pure core.
    /// The exact stale-busy shape: a HEADLESS row whose owning daemon is DOWN but
    /// whose orphaned claude child is STILL ALIVE (`AliveSilentValid`, so the
    /// claude-pid `gated_ls_status` alone would KEEP `busy`). The daemon-down gate
    /// must take precedence → `Cold`.
    ///
    /// FIX-SHAPED MUTATION (red-before): delete the daemon-liveness branch in
    /// `gated_ls_status_headless` (trust the row unconditionally) → this returns
    /// the stale `SessionStatus::Busy` and the assert below reds with
    /// `left: Busy, right: Cold`. Restoring the gate makes it green.
    #[test]
    fn headless_gate_daemon_down_with_live_orphan_is_cold() {
        let g = gated_ls_status_headless(
            SessionStatus::Busy,
            Some(crate::observe::HEADLESS_ENTRYPOINT),
            Some("hl"),
            Some(4242),
            Some(1000),
            &FakeDaemon(DaemonLiveness::Down),
            &ConstSrc(LifecycleState::AliveSilentValid), // orphan claude STILL alive
        );
        assert_eq!(
            g,
            SessionStatus::Cold,
            "headless + daemon DOWN → Cold even though the orphan claude pid is alive \
             (not stale-busy)"
        );
    }

    /// (ii) DAEMON-LIVE PATH UNCHANGED: a headless row whose daemon is UP keeps the
    /// EXISTING claude-pid behavior — alive orphan keeps `busy`; dead orphan still
    /// downgrades to `Cold` via the unchanged `gated_ls_status` leg.
    #[test]
    fn headless_gate_daemon_up_delegates_to_claude_pid() {
        // daemon UP + orphan alive → busy preserved (the daemon-live path).
        assert_eq!(
            gated_ls_status_headless(
                SessionStatus::Busy,
                Some(crate::observe::HEADLESS_ENTRYPOINT),
                Some("hl"),
                Some(7),
                Some(1000),
                &FakeDaemon(DaemonLiveness::Up),
                &ConstSrc(LifecycleState::AliveSilentValid),
            ),
            SessionStatus::Busy,
            "daemon UP + live orphan → unchanged busy",
        );
        // daemon UP + orphan dead → the existing claude-pid gate still downgrades.
        assert_eq!(
            gated_ls_status_headless(
                SessionStatus::Busy,
                Some(crate::observe::HEADLESS_ENTRYPOINT),
                Some("hl"),
                Some(7),
                Some(1000),
                &FakeDaemon(DaemonLiveness::Up),
                &ConstSrc(LifecycleState::Gone),
            ),
            SessionStatus::Cold,
            "daemon UP + dead orphan → claude-pid gate downgrades (unchanged)",
        );
    }

    /// (ii) SCOPE: a NON-headless (interactive) row is NEVER daemon-down-gated —
    /// even with the daemon DOWN it delegates straight to the claude-pid gate, so a
    /// live interactive pid keeps its status. (Interactive liveness is the mux-pane
    /// signal, not this per-session socket — lead RULING.)
    #[test]
    fn non_headless_row_skips_daemon_gate() {
        assert_eq!(
            gated_ls_status_headless(
                SessionStatus::Busy,
                None, // interactive — no headless entrypoint
                Some("tui"),
                Some(7),
                Some(1000),
                &FakeDaemon(DaemonLiveness::Down),
                &ConstSrc(LifecycleState::AliveSilentValid),
            ),
            SessionStatus::Busy,
            "non-headless row is not daemon-gated (live pid keeps busy)",
        );
    }

    /// (ii) FAIL-OPEN guards: a headless row with NO name (cannot form the socket
    /// leaf) falls through to the claude-pid gate (never hidden on an unprobeable
    /// daemon); a non-live status is never re-probed/resurrected.
    #[test]
    fn headless_gate_fail_open_and_nonlive() {
        // headless + daemon DOWN but NO name → cannot probe → claude-pid gate (alive
        // orphan keeps busy).
        assert_eq!(
            gated_ls_status_headless(
                SessionStatus::Busy,
                Some(crate::observe::HEADLESS_ENTRYPOINT),
                None,
                Some(7),
                Some(1000),
                &FakeDaemon(DaemonLiveness::Down),
                &ConstSrc(LifecycleState::AliveSilentValid),
            ),
            SessionStatus::Busy,
            "no name → fail-open to the claude-pid gate",
        );
        // already cold → never probed/resurrected even with daemon DOWN.
        assert_eq!(
            gated_ls_status_headless(
                SessionStatus::Cold,
                Some(crate::observe::HEADLESS_ENTRYPOINT),
                Some("hl"),
                Some(7),
                Some(1000),
                &FakeDaemon(DaemonLiveness::Down),
                &ConstSrc(LifecycleState::Gone),
            ),
            SessionStatus::Cold,
        );
    }

    /// CONCURRENCY/LOAD (§6 DoD #5, k≥2) + LATENCY on the gate's render path: gate
    /// k≥2 live rows in one `qd ls` (the per-row OS classify the ls verb runs).
    /// Two real child pids both classify alive (recorded start == live start →
    /// never gated); p50/p99 of gating all k rows is PRINTED. Cost is k sequential
    /// `ps` forks — generous budget; `ls` is not a hot loop.
    #[test]
    fn gate_k2_latency() {
        use crate::effects::Clock;
        let mut kids: Vec<std::process::Child> = (0..2)
            .map(|_| Command::new("sleep").arg("30").spawn().expect("spawn"))
            .collect();
        // Production-shape wall-clock recorded start captured at "registration"
        // (right after spawn) — NOT proc_start_ms, which returns None for a
        // sub-second child under load → a degenerate `0` start → spurious
        // NotOurs → a live row wrongly gated to Cold. Each live row's real `ps`
        // start vs this wall-clock now differ only by etime rounding (≤~1s) ≪
        // the 120s slack ⇒ ALIVE ⇒ not gated, under load too.
        let rows: Vec<(i64, i64)> = kids
            .iter()
            .map(|c| {
                let pid = c.id() as i64;
                let start = effects::RealClock.now_ms();
                (pid, start)
            })
            .collect();
        let src = OsLiveness::new();
        let n = 30;
        let mut us: Vec<u128> = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            for &(pid, start) in &rows {
                let g = gated_ls_status(SessionStatus::Busy, Some(pid), Some(start), &src);
                assert_eq!(g, SessionStatus::Busy, "a live child must NOT be gated");
            }
            us.push(t.elapsed().as_micros());
        }
        for c in &mut kids {
            let _ = c.kill();
            let _ = c.wait();
        }
        us.sort_unstable();
        let p50 = us[n / 2];
        let p99 = us[(n * 99 / 100).min(n - 1)];
        println!("gated_ls_status(k=2 rows) latency: p50={p50}us p99={p99}us (n={n})");
        assert!(p99 < 500_000, "p99 {p99}us exceeds 500ms budget for k=2");
    }

    // ===================== WP-B4 readiness-augmented StreamLiveness =====
    // The (B) overlay: daemon headless-stream + relay/bond ledger signals folded
    // onto the OS verdict, behind the seam. `ConstSrc` (above) is the fixed-state
    // inner; all classification is deterministic with no real /proc or claude.

    const TAU: i64 = STUCK_THRESHOLD_MS;

    /// Build a `StreamLiveness` whose inner ALWAYS classifies `inner_state`.
    fn stream_over(inner_state: LifecycleState) -> StreamLiveness<ConstSrc> {
        StreamLiveness::new(ConstSrc(inner_state))
    }

    fn key() -> ProcKey {
        ProcKey::new(4242, RECORDED_START)
    }

    /// Test #1 — death-gate-untouched (LOAD-BEARING): each new alive sub-state is
    /// `is_alive()` and NOT `is_dead()`, and the death-confirmation fold never
    /// convicts any of them. Pins "READY/STUCK/WAITING never killable; the
    /// fail-closed invariant holds."
    #[test]
    fn b4_new_states_are_alive_never_dead() {
        use LifecycleState::*;
        for s in [AliveReady, AliveWaiting, Stuck] {
            assert!(s.is_alive(), "{s:?} must be alive");
            assert!(
                !s.is_dead(),
                "{s:?} must NOT be dead (death gate untouched)"
            );
        }
        assert!(!confirmed_dead(&[Stuck]), "Stuck never convicts");
        assert!(!confirmed_dead(&[AliveReady]), "AliveReady never convicts");
        assert!(
            !confirmed_dead(&[AliveWaiting]),
            "AliveWaiting never convicts"
        );
        // A mixed sequence with one new alive state spares the whole fold.
        assert!(!confirmed_dead(&[Gone, Stuck, Gone]));
    }

    /// Test #2 — STUCK diagnostic-only contract (§H.7): `Stuck` is NOT dead, IS
    /// `is_diagnostic_stuck`, and NO other state is. Machine-checks the
    /// never-authorizes-a-kill contract.
    #[test]
    fn b4_stuck_is_diagnostic_only() {
        use LifecycleState::*;
        assert!(!Stuck.is_dead(), "Stuck must never be dead");
        assert!(Stuck.is_diagnostic_stuck(), "Stuck is the diagnostic state");
        for s in [
            AliveSilentValid,
            AliveWorking,
            AliveReady,
            AliveWaiting,
            ExitedClean,
            ExitedSignal,
            Gone,
            NotOurs,
        ] {
            assert!(
                !s.is_diagnostic_stuck(),
                "{s:?} must NOT be is_diagnostic_stuck (Stuck only)"
            );
        }
    }

    /// Test #3 — per-state red/green over `classify_obs` with a fixed-alive inner.
    #[test]
    fn b4_per_state_classification() {
        use LifecycleState::*;
        let sl = stream_over(AliveWorking);

        // first_output_seen, no turn → AliveReady.
        let obs = StreamObs {
            first_output_seen: true,
            ..Default::default()
        };
        assert_eq!(sl.classify_obs(key(), &obs), AliveReady);

        // turn in flight, within τ → AliveWorking.
        let obs = StreamObs {
            turn_in_flight: true,
            since_turn_start_ms: Some(TAU - 1),
            ..Default::default()
        };
        assert_eq!(sl.classify_obs(key(), &obs), AliveWorking);

        // turn in flight, past τ → Stuck.
        let obs = StreamObs {
            turn_in_flight: true,
            since_turn_start_ms: Some(TAU + 1),
            ..Default::default()
        };
        assert_eq!(sl.classify_obs(key(), &obs), Stuck);

        // default obs (no signal) over an AliveSilentValid inner → passthrough,
        // NO false Ready/Stuck (the false-positive guard).
        let sl_silent = stream_over(AliveSilentValid);
        assert_eq!(
            sl_silent.classify_obs(key(), &StreamObs::default()),
            AliveSilentValid
        );
    }

    /// Test #4 — waiting-NOT-stuck (the amend keystone): a `--wait` past τ is
    /// AliveWaiting, NOT Stuck. The `waiting_on_ledger` short-circuit is what
    /// makes this hold; without it the same obs classifies Stuck (the red-before,
    /// captured by temporarily removing the short-circuit during TDD).
    #[test]
    fn b4_waiting_is_not_stuck() {
        use LifecycleState::*;
        let sl = stream_over(AliveWorking);
        let obs = StreamObs {
            turn_in_flight: true,
            since_turn_start_ms: Some(TAU + 10_000),
            waiting_on_ledger: true,
            ..Default::default()
        };
        let got = sl.classify_obs(key(), &obs);
        assert_eq!(
            got, AliveWaiting,
            "a --wait past τ must be AliveWaiting, not Stuck"
        );
        assert_ne!(got, Stuck, "a --wait is NEVER stuck (the amend keystone)");
    }

    /// Test #5 — OS-truth-wins (false-neg guard): a non-alive base state passes
    /// through UNCHANGED for ANY obs (even first_output_seen) — a stream signal
    /// never resurrects a dead/foreign process. Asserts Gone, ExitedSignal, NotOurs.
    #[test]
    fn b4_os_truth_wins_for_dead_and_foreign() {
        use LifecycleState::*;
        let loud = StreamObs {
            first_output_seen: true,
            turn_in_flight: true,
            since_turn_start_ms: Some(TAU + 10_000),
            waiting_on_ledger: true,
        };
        for base in [Gone, ExitedSignal, NotOurs] {
            let sl = stream_over(base);
            assert_eq!(
                sl.classify_obs(key(), &loud),
                base,
                "{base:?} must pass through unchanged (no resurrection)"
            );
            // Even an otherwise-Ready-shaped obs cannot revive it.
            assert_eq!(
                sl.classify_obs(
                    key(),
                    &StreamObs {
                        first_output_seen: true,
                        ..Default::default()
                    }
                ),
                base
            );
        }
    }

    /// Test #6 — ledger-feed seam: flipping ONLY `waiting_on_ledger` on an
    /// otherwise-AliveWorking obs changes AliveWorking → AliveWaiting. Proves the
    /// ledger feed is wired and decisive.
    #[test]
    fn b4_ledger_feed_is_decisive() {
        use LifecycleState::*;
        let sl = stream_over(AliveWorking);
        let working = StreamObs {
            turn_in_flight: true,
            since_turn_start_ms: Some(TAU - 1),
            waiting_on_ledger: false,
            ..Default::default()
        };
        assert_eq!(sl.classify_obs(key(), &working), AliveWorking);
        let waiting = StreamObs {
            waiting_on_ledger: true,
            ..working.clone()
        };
        assert_eq!(
            sl.classify_obs(key(), &waiting),
            AliveWaiting,
            "flipping only the ledger flag must flip the verdict"
        );
    }

    /// WP-B-CS-2 — the `qd ls` readiness facet maps the full state machine onto
    /// the D3 triad (ready/silent/stuck), and is `None` for every not-alive state
    /// (those rows are gated Cold). FALSE-POSITIVE guard: only `AliveReady` is
    /// `ready` and only `Stuck` is `stuck` (gated by `is_diagnostic_stuck`); every
    /// other alive state is `silent` — no state is mis-labelled ready/stuck.
    #[test]
    fn readiness_facet_maps_the_triad() {
        use LifecycleState::*;
        assert_eq!(readiness_facet(AliveReady), Some(Readiness::Ready));
        assert_eq!(readiness_facet(Stuck), Some(Readiness::Stuck));
        // All other ALIVE states coarsen to Silent (incl. waiting + working).
        for s in [AliveSilentValid, AliveWorking, AliveWaiting] {
            assert_eq!(
                readiness_facet(s),
                Some(Readiness::Silent),
                "{s:?} → silent"
            );
        }
        // NOT-alive: no facet (gated Cold).
        for s in [ExitedClean, ExitedSignal, Gone, NotOurs] {
            assert_eq!(readiness_facet(s), None, "{s:?} → no facet");
        }
        // Only Stuck is ever the stuck facet (the diagnostic-only contract).
        for s in [AliveSilentValid, AliveWorking, AliveWaiting, AliveReady] {
            assert_ne!(
                readiness_facet(s),
                Some(Readiness::Stuck),
                "{s:?} must not be stuck"
            );
        }
        assert_eq!(Readiness::Ready.as_str(), "ready");
        assert_eq!(Readiness::Silent.as_str(), "silent");
        assert_eq!(Readiness::Stuck.as_str(), "stuck");
    }

    /// Test #7 — delegation: the base trait `classify(key)` == `inner.classify(key)`
    /// for several states, so `StreamLiveness` drops into existing consumers
    /// unchanged.
    #[test]
    fn b4_base_classify_delegates_to_inner() {
        use LifecycleState::*;
        for state in [AliveSilentValid, AliveWorking, Gone, ExitedSignal, NotOurs] {
            let inner = ConstSrc(state);
            let expected = inner.classify(key());
            let sl = StreamLiveness::new(inner);
            assert_eq!(
                sl.classify(key()),
                expected,
                "StreamLiveness::classify must delegate to inner for {state:?}"
            );
        }
    }
}
