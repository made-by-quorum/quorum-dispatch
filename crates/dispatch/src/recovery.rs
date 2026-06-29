//! R3c-Step-2 — the four-rung recovery ladder + its durable substrate.
//!
//! This module is the ladder's BRAIN: the pure, unit-testable decision logic (which
//! rung, how far the safe-by-default cap permits, when to back off, how to resume
//! after a coordinator crash) plus the durable recovery row that survives a restart.
//! The actual syscalls of each rung (pidfd-signal, control-wake, PTY-inject,
//! checkpoint+respawn) live behind the coordinator's IO seam
//! (`bin/recovery_coordinator.rs`) so the brain stays testable in isolation.
//!
//! ## The ladder (R1 §6)
//! Rung 1 pidfd-signal (5s) → Rung 2 control-wake (10s) → Rung 3 PTY-inject (15s) →
//! Rung 4 checkpoint+respawn (≈96s, two-phase crash-idempotent). A wedged-but-alive
//! session climbs from Rung 1; a dead-dangling sender enters at Rung 4 directly
//! (OQ#9 — no point waking a dead process).
//!
//! ## The keystones
//! - **Safe-by-default cap (Pete, binding):** the destructive Rung 4 fires ONLY
//!   when signal-B is LIVE for the session's harness ([`ladder_ceiling`]); an
//!   advisory-only harness caps below Rung 4.
//! - **Crash-idempotent Rung 4 (P1.1):** a two-phase intent record on the recovery
//!   row, re-verified by the successor's OWN identity on restart — NEVER blind
//!   re-issue ([`resume_action`]).
//! - **Lease, not a bare PID (P5/deepseek/grok):** [`LadderOwner`] is fenced on
//!   `coordinator_incarnation`; a restarted coordinator steals an EXPIRED lease.
//! - **Cascade-death prevention (§5.2):** a fleet-wide respawn semaphore of 1 + a
//!   host-load/memory back-off gate ([`respawn_admission`]) + the D-state backstop
//!   ([`d_state_blocks_respawn`]).

use crate::health::{classify_health, Health};
use crate::liveness::{DaemonLiveness, LifecycleState};
use crate::model::SessionStatus;
use crate::registry::{self, TombstoneOutcome};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ===========================================================================
// Rungs + ladder state
// ===========================================================================

/// The four rungs, in escalation order (1..4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Rung {
    /// Rung 1 — `pidfd_open` then `pidfd_send_signal(SIGCONT)` (RF-4: open-then-send,
    /// the fd binding is the safety guarantee). Non-destructive.
    PidfdSignal,
    /// Rung 2 — `send_control(WakeInbox)` + `send_control(Checkpoint)`. The fast path
    /// that recovers most wedges under design (A). Non-destructive.
    ControlWake,
    /// Rung 3 — `decide_send_pty`→`SendVerify` PTY inject (newline / `\x03`).
    /// Non-destructive.
    PtyInject,
    /// Rung 4 — checkpoint + SIGKILL + respawn. DESTRUCTIVE; gated on the
    /// safe-by-default cap (live signal-B) + the throttle/back-off + the D-state
    /// backstop, and crash-idempotent (two-phase intent record).
    Respawn,
}

impl Rung {
    /// 1-based escalation level.
    pub fn level(self) -> u8 {
        match self {
            Rung::PidfdSignal => 1,
            Rung::ControlWake => 2,
            Rung::PtyInject => 3,
            Rung::Respawn => 4,
        }
    }

    /// Per-rung deadline in ms (R1 §6). Rung 4 = 60s confirm + 3s SIGTERM grace + 3s
    /// SIGKILL grace ≈ 96s.
    pub fn deadline_ms(self) -> i64 {
        match self {
            Rung::PidfdSignal => 5_000,
            Rung::ControlWake => 10_000,
            Rung::PtyInject => 15_000,
            Rung::Respawn => 96_000,
        }
    }

    /// The non-destructive rungs (1–3) are safe to re-arm on a coordinator restart
    /// (idempotent / terminal-less); only Rung 4 needs the crash-idempotent dance.
    pub fn is_destructive(self) -> bool {
        matches!(self, Rung::Respawn)
    }

    /// The next rung up, or `None` at the top (Rung 4).
    pub fn escalate(self) -> Option<Rung> {
        match self {
            Rung::PidfdSignal => Some(Rung::ControlWake),
            Rung::ControlWake => Some(Rung::PtyInject),
            Rung::PtyInject => Some(Rung::Respawn),
            Rung::Respawn => None,
        }
    }

    /// Lowercase wire/display token (for the event log).
    pub fn as_str(self) -> &'static str {
        match self {
            Rung::PidfdSignal => "pidfd-signal",
            Rung::ControlWake => "control-wake",
            Rung::PtyInject => "pty-inject",
            Rung::Respawn => "respawn",
        }
    }
}

/// The ladder's high-level state, mirrored into the durable row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LadderState {
    /// No recovery in flight.
    Idle,
    /// A rung is in flight.
    Running(Rung),
    /// Terminal: 3 CONFIRMED consecutive failures, or a D-state target — operator
    /// alert, no further automation.
    Crit,
}

/// CONFIRMED consecutive failures that escalate to [`LadderState::Crit`] (R1 §6).
pub const CRIT_CONSECUTIVE_FAILURES: u32 = 3;

// ===========================================================================
// Triggers (consume already-computed signals; never re-derive them)
// ===========================================================================

/// What the ladder should do, decided from the consumed signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Healthy / non-actionable — do nothing.
    None,
    /// Arm the ladder from Rung 1 (a wedged-but-alive session climbs from the top).
    Arm,
    /// Enter at Rung 4 directly (a dead-dangling / abandoned sender — OQ#9: no point
    /// waking a dead process). STILL subject to the safe-by-default cap.
    EnterAtRespawn,
}

/// Decide the ladder trigger from already-computed signals — PURE.
///
/// - `is_dead_dangling` (the start_ms-guarded `events::is_dead_dangling`, R3d) ⇒
///   Rung 4 directly (OQ#9).
/// - `Health::Wedged` (`health.authorizes_recovery()`) ⇒ arm from Rung 1.
/// - `connect_ok == Some(false)` for a session that SHOULD be running ⇒ arm (the
///   channel-down trigger, `wait_channel`).
///
/// Death (`Health::Dead`) is the kill gate's domain, not the wedge ladder's — it is
/// NOT a trigger here (a genuinely dead session is reaped, not respawned-in-place).
pub fn ladder_trigger(
    health: Health,
    connect_ok: Option<bool>,
    is_dead_dangling: bool,
    should_be_running: bool,
) -> Trigger {
    if is_dead_dangling {
        return Trigger::EnterAtRespawn;
    }
    if health.authorizes_recovery() {
        return Trigger::Arm;
    }
    if should_be_running && connect_ok == Some(false) {
        return Trigger::Arm;
    }
    Trigger::None
}

// ===========================================================================
// The safe-by-default destructive cap (Pete, BINDING)
// ===========================================================================

/// The ladder's ceiling for this session's harness. The destructive Rung 4
/// (SIGKILL+respawn) fires ONLY when signal-B is LIVE for the harness; an
/// advisory-only harness (no trusted live signal-B — e.g. `--sparse-output` until N
/// is calibrated) caps at Rung 3 (PTY-inject), NEVER reaching SIGKILL+respawn. This
/// is NEVER a per-session human flag — it is a property of whether the harness wires
/// a trustworthy signal-B producer.
pub fn ladder_ceiling(signal_b_harness_live: bool) -> Rung {
    if signal_b_harness_live {
        Rung::Respawn
    } else {
        Rung::PtyInject
    }
}

/// Is the destructive Rung 4 authorized for this harness? (The cap, as a predicate.)
pub fn destructive_authorized(signal_b_harness_live: bool) -> bool {
    ladder_ceiling(signal_b_harness_live) == Rung::Respawn
}

/// Clamp a desired rung to the safe-by-default ceiling. An advisory-only harness's
/// ladder that would climb to Rung 4 is held at Rung 3.
pub fn cap_rung(desired: Rung, signal_b_harness_live: bool) -> Rung {
    desired.min(ladder_ceiling(signal_b_harness_live))
}

// ===========================================================================
// Cascade-death-spiral prevention (§5.2)
// ===========================================================================

/// The fleet-wide cap on concurrent Rung-4 respawns (§5.2 item 1). Respawn is the
/// expensive rung — serialize it so a correlated wedge cannot stampede into an OOM.
pub const RESPAWN_SEMAPHORE_MAX: usize = 1;

/// A host-load snapshot the back-off gate reads (`/proc/loadavg` + available memory).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostLoad {
    /// 1-minute load average (`/proc/loadavg` field 1).
    pub loadavg_1m: f64,
    /// Available memory in bytes (`MemAvailable`).
    pub avail_mem_bytes: u64,
    /// CPU count (the load ceiling scales with it).
    pub ncpu: usize,
    /// Rung-4 respawns currently in flight fleet-wide (the global semaphore count).
    pub respawns_in_flight: usize,
}

/// Why a Rung-4 respawn was admitted or suspended (§5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespawnAdmission {
    /// Proceed — host has headroom and the semaphore is free.
    Admit,
    /// Semaphore full: another Rung-4 respawn is already in flight (≤1 fleet-wide).
    SuspendConcurrency,
    /// Available memory below the floor — assume host congestion, not agent defects.
    SuspendLowMem,
    /// Load average above the per-CPU ceiling — back off (correlated wedge ⇒ host).
    SuspendHighLoad,
}

impl RespawnAdmission {
    /// True only for [`RespawnAdmission::Admit`].
    pub fn is_admitted(self) -> bool {
        matches!(self, RespawnAdmission::Admit)
    }
}

/// Host-load back-off gate before EVERY Rung 4 (§5.2 items 1+2). Checks, in order:
/// the global respawn semaphore (≤1), available memory, and load average. A wedge
/// correlated across many sessions is far likelier host-wide than N independent
/// agent bugs — so under pressure we SUSPEND respawns rather than amplify it.
pub fn respawn_admission(
    load: &HostLoad,
    min_avail_bytes: u64,
    load_per_cpu_ceiling: f64,
) -> RespawnAdmission {
    if load.respawns_in_flight >= RESPAWN_SEMAPHORE_MAX {
        return RespawnAdmission::SuspendConcurrency;
    }
    if load.avail_mem_bytes < min_avail_bytes {
        return RespawnAdmission::SuspendLowMem;
    }
    if load.ncpu > 0 && load.loadavg_1m > load_per_cpu_ceiling * load.ncpu as f64 {
        return RespawnAdmission::SuspendHighLoad;
    }
    RespawnAdmission::Admit
}

/// D-state backstop (§R3c-Step-2 / gemini §4): a process in uninterruptible-sleep
/// `D` ignores `SIGKILL`, and a respawn would re-enter `D` (a pile-up of unkillable
/// processes). A `D` target is NEVER respawn-looped → CRIT + host alarm. The `State:`
/// char comes from `/proc/<pid>/status` (read by the coordinator).
pub fn d_state_blocks_respawn(proc_state: char) -> bool {
    proc_state == 'D'
}

// ===========================================================================
// Identity + two-phase intent record (crash-idempotent Rung 4, P1)
// ===========================================================================

/// A session incarnation's full identity. `(pid, start_ms)` is the reuse-robust OS
/// identity; `incarnation` is the monotonic claim fence (registry `claim_incarnation`).
/// The two-phase Rung-4 record captures the OLD identity so a restarted coordinator
/// can distinguish "my successor is healthy" (incarnation advanced) from "respawn
/// never completed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    pub start_ms: i64,
    pub incarnation: u64,
}

/// The two-phase Rung-4 commit phases. Each irreversible step is preceded by an
/// atomic intent-record write so a coordinator crash mid-respawn cannot SIGKILL the
/// healthy successor (P1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// No Rung-4 transition in flight.
    Idle,
    /// Intent recorded; about to SIGTERM→SIGKILL the old identity.
    Killing,
    /// Old identity tombstoned (CAS-guarded); about to spawn.
    Tombstoned,
    /// Successor session_id minted + recorded; spawning the fresh session.
    Spawning,
    /// Spawned; awaiting the successor's `AliveReady`.
    Confirming,
}

// ===========================================================================
// Lease (time-bounded, fenced on coordinator_incarnation — NOT a bare PID)
// ===========================================================================

/// The per-session ladder lease (R1 §6 inv 4; deepseek/grok lease finding). A
/// restarted coordinator detecting an EXPIRED lease STEALS it (closing the
/// dead-owner-blocks-recovery-forever deadlock); `coordinator_incarnation` (monotonic,
/// NOT a bare PID) fences two live coordinators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LadderOwner {
    pub coordinator_incarnation: u64,
    pub expiry_ms: i64,
}

impl LadderOwner {
    /// A fresh lease: `expiry = now + max_rung_deadline + grace`.
    pub fn new(coordinator_incarnation: u64, now_ms: i64, grace_ms: i64) -> Self {
        LadderOwner {
            coordinator_incarnation,
            expiry_ms: now_ms + Rung::Respawn.deadline_ms() + grace_ms,
        }
    }

    /// Has the lease expired (a restarted coordinator may steal it)?
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expiry_ms
    }
}

/// May `my_incarnation` acquire/hold the ladder lease now? Yes if unowned, if the
/// current lease is EXPIRED (steal), or if I already own it (re-entrant). A LIVE
/// lease owned by a DIFFERENT coordinator fences me out (no two live coordinators).
pub fn can_acquire_lease(
    current: Option<&LadderOwner>,
    my_incarnation: u64,
    now_ms: i64,
) -> bool {
    match current {
        None => true,
        Some(o) => o.is_expired(now_ms) || o.coordinator_incarnation == my_incarnation,
    }
}

// ===========================================================================
// The durable recovery row
// ===========================================================================

/// Recovery-row schema version (grok: a corrupt/older row is treated as cold, never
/// bricks a session's recovery).
pub const RECOVERY_ROW_VERSION: u32 = 1;

/// The durable per-session recovery row, `<state_dir>/recovery/<session_id>.json`
/// (R1 §5/§5.1). Atomic tmp+rename; a parse failure ⇒ treat as cold (grok).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRow {
    /// Schema version (grok corrupt-row resilience).
    pub version: u32,
    pub session_id: String,
    /// The lease — `None` when no coordinator holds the ladder.
    pub ladder_owner: Option<LadderOwner>,
    pub ladder_state: LadderState,
    /// The two-phase Rung-4 phase (`Idle` outside a destructive transition).
    pub phase: Phase,
    /// The OLD identity a Rung-4 transition is acting on (P1.1 fence).
    pub old_identity: Option<Identity>,
    /// The successor session_id minted by a Rung-4 respawn (P8 `session-replaced`).
    pub new_session_id: Option<String>,
    /// CONFIRMED consecutive failures — incremented ONLY on a confirmed failed
    /// recovery, READ (not reset) on coordinator boot (RF-3).
    pub consecutive_failures: u32,
    pub last_attempt_ms: Option<i64>,
    /// The persisted daemon-up instant (the daemon-down grace window, Open-Q #7).
    pub daemon_up_ms: Option<i64>,
    /// Per-session τ override (Open-Q #6).
    pub tau_override_ms: Option<i64>,
}

impl RecoveryRow {
    /// A fresh, cold row for `session_id`.
    pub fn cold(session_id: &str) -> Self {
        RecoveryRow {
            version: RECOVERY_ROW_VERSION,
            session_id: session_id.to_string(),
            ladder_owner: None,
            ladder_state: LadderState::Idle,
            phase: Phase::Idle,
            old_identity: None,
            new_session_id: None,
            consecutive_failures: 0,
            last_attempt_ms: None,
            daemon_up_ms: None,
            tau_override_ms: None,
        }
    }
}

/// The recovery-row path: `<state_dir>/recovery/<session_id>.json`.
pub fn recovery_row_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("recovery")
        .join(format!("{session_id}.json"))
}

/// Read a recovery row. A missing file ⇒ `None` (cold). A CORRUPT/unparseable row
/// (ENOSPC/EDQUOT partial write, or an older schema) ⇒ `None` (treated as cold,
/// grok) — never silently stuck on a corrupt row.
pub fn read_recovery_row(state_dir: &Path, session_id: &str) -> Option<RecoveryRow> {
    let path = recovery_row_path(state_dir, session_id);
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<RecoveryRow>(&bytes) {
        Ok(row) => Some(row),
        Err(e) => {
            eprintln!(
                "recovery: corrupt row {path:?} ({e}); treating session as cold (re-deriving \
                 from presence)"
            );
            None
        }
    }
}

/// Write a recovery row atomically (tmp+rename, the registry.rs:517 discipline). The
/// rename is atomic, so a reader never sees a torn row.
pub fn write_recovery_row(state_dir: &Path, row: &RecoveryRow) -> io::Result<()> {
    let dir = state_dir.join("recovery");
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join(format!("{}.json", row.session_id));
    let tmp = dir.join(format!(".{}.json.tmp.{}", row.session_id, std::process::id()));
    let bytes = serde_json::to_vec(row).map_err(io::Error::other)?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(())
}

// ===========================================================================
// Crash-resume decision (P1.1 — the load-bearing fix)
// ===========================================================================

/// What a restarted coordinator should do with a recovery row it finds (after
/// stealing an expired lease). The destructive in-flight cases NEVER blind re-issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// Nothing in flight — evaluate current health fresh.
    Cold,
    /// A non-destructive rung (1–3) was in flight: safe to re-arm (idempotent).
    ReArm(Rung),
    /// Rung 4 was in flight and a successor was minted, but the successor is NOT
    /// confirmed healthy — re-verify it by its OWN identity and re-act ONLY if it is
    /// genuinely absent/wedged.
    ReverifySuccessor { new_session_id: String },
    /// Rung 4 was in flight before a successor was minted (Killing/Tombstoned) —
    /// re-evaluate CURRENT health, then resume the kill→spawn if still wedged.
    ResumeKill { old_identity: Option<Identity> },
    /// The successor's incarnation advanced — it is healthy. Clear the row, do
    /// nothing (NEVER re-kill a healthy successor — the P1.1 catastrophe).
    SuccessorHealthy,
}

/// Decide the resume action for an in-flight row on coordinator restart (P1.1/P1.2).
///
/// `successor_healthy` is the coordinator's verdict from re-probing the successor by
/// its OWN `(pid, start_ms, incarnation)` via the presence module — passed in so this
/// decision stays pure. The rule: a healthy successor ⇒ clear (never re-kill); a
/// post-mint phase with an unhealthy successor ⇒ re-verify (re-act only if genuinely
/// gone); a pre-mint phase ⇒ resume the kill after re-evaluating CURRENT health.
pub fn resume_action(row: &RecoveryRow, successor_healthy: bool) -> ResumeAction {
    match row.phase {
        Phase::Idle => match row.ladder_state {
            // A non-destructive rung in flight ⇒ safe to re-arm.
            LadderState::Running(rung) if !rung.is_destructive() => ResumeAction::ReArm(rung),
            _ => ResumeAction::Cold,
        },
        // Pre-mint destructive phases: no successor exists yet → resume the kill,
        // but the caller re-evaluates CURRENT health first (P1.2).
        Phase::Killing | Phase::Tombstoned => ResumeAction::ResumeKill {
            old_identity: row.old_identity.clone(),
        },
        // Post-mint destructive phases: a successor was (being) minted.
        Phase::Spawning | Phase::Confirming => {
            if successor_healthy {
                // The incarnation fence says the successor is alive → do nothing.
                ResumeAction::SuccessorHealthy
            } else {
                match &row.new_session_id {
                    Some(nsid) => ResumeAction::ReverifySuccessor {
                        new_session_id: nsid.clone(),
                    },
                    // Spawning recorded but no session_id yet → re-evaluate + resume.
                    None => ResumeAction::ResumeKill {
                        old_identity: row.old_identity.clone(),
                    },
                }
            }
        }
    }
}

// ===========================================================================
// Strike counter (increment on CONFIRMED failure, never reset on boot — RF-3)
// ===========================================================================

/// The outcome of one recovery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The session recovered (status left `busy` / progress advanced / successor
    /// `AliveReady`). Reset the strike counter.
    Recovered,
    /// A CONFIRMED failed recovery (the rung's deadline elapsed with no recovery).
    /// Increment the strike counter.
    ConfirmedFailure,
    /// Indeterminate (an in-flight attempt interrupted by a coordinator crash) — do
    /// NOT touch the strike counter; re-evaluate CURRENT health (P1.2).
    Indeterminate,
}

/// Apply a recovery outcome to the strike counter (RF-3). `Recovered` resets to 0;
/// `ConfirmedFailure` increments; `Indeterminate` leaves it UNTOUCHED. Returns the
/// new count and whether CRIT is reached (3 confirmed failures).
pub fn apply_outcome(consecutive_failures: u32, outcome: RecoveryOutcome) -> (u32, bool) {
    let next = match outcome {
        RecoveryOutcome::Recovered => 0,
        RecoveryOutcome::ConfirmedFailure => consecutive_failures.saturating_add(1),
        RecoveryOutcome::Indeterminate => consecutive_failures,
    };
    (next, next >= CRIT_CONSECUTIVE_FAILURES)
}

// ===========================================================================
// The coordinator's rung IO seam (the thin syscall edge over the pure brain)
// ===========================================================================

/// Rung 1 — pidfd-signal (RF-4). `pidfd_open(pid, 0)` then
/// `pidfd_send_signal(pidfd, sig)`: open-then-send with NO re-probe between (the fd
/// binding to THIS pid is the reuse-safety guarantee; the `(pid, start_ms)` ownership
/// gate is a SEPARATE check done before any registry WRITE, never between open and
/// send — no TOCTOU). `ESRCH` from `pidfd_open` ⇒ the target is already dead.
///
/// Linux uses the raw `pidfd` syscalls (reuse-safe). Non-Linux targets (macOS dev
/// boxes) have no `pidfd`; see the `cfg(not(target_os = "linux"))` fallback below.
#[cfg(target_os = "linux")]
pub fn rung1_pidfd_signal(pid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: raw pidfd syscalls. `pidfd_open` returns an fd bound to THIS pid
    // (reuse-safe); we close it after the send on every path.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
    if pidfd < 0 {
        return Err(io::Error::last_os_error()); // ESRCH = already-dead
    }
    let pidfd = pidfd as libc::c_int;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            sig as libc::c_int,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    let send_err = io::Error::last_os_error();
    // SAFETY: closing our own valid pidfd.
    unsafe {
        libc::close(pidfd);
    }
    if rc < 0 {
        return Err(send_err);
    }
    Ok(())
}

/// Non-Linux (macOS) Rung-1 fallback: there is no `pidfd`, so signal by pid via
/// `kill(2)`. The pidfd reuse-safety guarantee is unavailable, but the caller's
/// `(pid, start_ms)` ownership gate (performed before any registry WRITE, never
/// between open and send) is the actual reuse fence — so a plain `kill` of a
/// SIGCONT liveness-nudge is equivalent in practice here. `ESRCH` ⇒ already-dead,
/// matching the Linux `pidfd_open` ESRCH path.
#[cfg(not(target_os = "linux"))]
pub fn rung1_pidfd_signal(pid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: kill(2) takes a pid + signal by value; no memory effects.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig as libc::c_int) };
    if rc < 0 {
        return Err(io::Error::last_os_error()); // ESRCH = already-dead
    }
    Ok(())
}

/// The harmless Rung-1 nudge: `SIGCONT` resumes a stopped process and is a no-op on a
/// running one — success means "the target is live and was signalled" (liveness then
/// advances or the turn ends).
pub const RUNG1_SIGNAL: i32 = libc::SIGCONT;

// ===========================================================================
// Rung 4 — the DESTRUCTIVE executor (two-phase, crash-idempotent, identity-fenced)
// ===========================================================================

/// The injected effects the destructive Rung-4 executor needs, behind the IO seam so
/// it is TESTABLE against an isolated throwaway victim WITHOUT the full daemon. The
/// production wiring supplies: `kill` = pidfd-SIGKILL ([`rung1_pidfd_signal`] with
/// `SIGKILL`); `current_start_ms` = [`crate::effects::proc_start_ms`] (the identity
/// re-fence); `mint_session_id` = [`crate::idstore::mint_unbound`] (a FRESH,
/// never-reused id); `spawn_successor` = the real daemon spawner; `confirm_ready` =
/// the presence/liveness `AliveReady` probe by the successor's own identity.
pub struct Rung4Io<'a> {
    /// SIGKILL the old identity by pid (pidfd-bound, reuse-safe). `ESRCH` (already
    /// dead) is treated as success by the executor.
    pub kill: &'a dyn Fn(i32) -> io::Result<()>,
    /// The current OS start-time (epoch ms) of `pid`, for the pre-SIGKILL identity
    /// re-fence. `None` ⇒ the pid is already gone (kill becomes a harmless no-op).
    pub current_start_ms: &'a dyn Fn(i32) -> Option<i64>,
    /// Mint a FRESH session_id (NON-reuse — a respawn never resurrects the dead id).
    pub mint_session_id: &'a dyn Fn() -> io::Result<String>,
    /// Spawn the successor session for the minted id; returns its pid.
    pub spawn_successor: &'a dyn Fn(&str) -> io::Result<i32>,
    /// Confirm the successor is `AliveReady` (by its OWN pid + session_id).
    pub confirm_ready: &'a dyn Fn(i32, &str) -> bool,
}

/// The terminal outcome of a destructive Rung-4 execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rung4Outcome {
    /// SIGKILL → tombstone → mint → spawn → CONFIRMED `AliveReady`: recovery
    /// succeeded. The successor's fresh `new_session_id` (≠ the dead id) + pid.
    Recovered {
        new_session_id: String,
        successor_pid: i32,
    },
    /// The pre-SIGKILL identity fence FAILED — the live pid's start-time no longer
    /// matches the recorded identity (a recycled pid). ABORT without killing: this
    /// is NOT the wedged session we decided to terminate (never SIGKILL a stranger).
    AbortedIdentityMismatch,
    /// The CAS-guarded tombstone REFUSED — a NEW incarnation reused the PID's row
    /// during the window. ABORT: never tombstone a live successor (§3 P5.2).
    AbortedReusedPidRow { on_disk_started_at: Option<i64> },
    /// The successor spawn failed — a CONFIRMED failed recovery (caller bumps strikes).
    SpawnFailed { new_session_id: String },
    /// Spawned but the successor never reached `AliveReady` within the budget — a
    /// CONFIRMED failed recovery (caller bumps strikes; the durable phase stays
    /// `Confirming` so a restart re-verifies, never blind re-kills).
    SuccessorUnconfirmed {
        new_session_id: String,
        successor_pid: i32,
    },
}

/// Execute the destructive Rung-4 sequence — `SIGKILL → CAS-tombstone → mint fresh
/// session_id → spawn → confirm AliveReady` — TWO-PHASE and crash-idempotent.
///
/// The durable recovery row is written BEFORE each irreversible step (Killing →
/// Tombstoned → Spawning → Confirming), so a coordinator crash mid-sequence resumes
/// via [`resume_action`] and NEVER blind re-kills a healthy successor (P1.1). Two
/// identity fences guard the kill: (1) a pre-SIGKILL OS start-time re-check (a
/// recycled pid aborts WITHOUT a kill), and (2) the CAS-guarded tombstone (a
/// reused-PID live row aborts WITHOUT destroying the successor, §3 P5.2). It targets
/// ONLY `old.pid`; a caller that passes an isolated throwaway victim's identity
/// cannot reach a fleet pid (the fences make the target unforgeable).
///
/// SAFE-BY-DEFAULT (Pete's keystone): reaching here is the CALLER's responsibility —
/// the cap ([`destructive_authorized`]) admits a destructive rung ONLY for a
/// signal-B-live wedge. This executor performs NO authorization; it is the syscall
/// edge BELOW the cap. `confirm_budget` is the `AliveReady` grace (≈
/// [`Rung::Respawn`] `deadline_ms` in production).
#[allow(clippy::too_many_arguments)]
pub fn execute_rung4(
    state_dir: &Path,
    sessions_dir: &Path,
    mut row: RecoveryRow,
    old: &Identity,
    old_row_started_at: Option<i64>,
    confirm_budget: Duration,
    confirm_poll: Duration,
    io: &Rung4Io,
) -> io::Result<Rung4Outcome> {
    // PHASE 1 — Killing. Record the intent BEFORE the irreversible SIGKILL so a crash
    // here resumes as ResumeKill (re-evaluate CURRENT health), never a blind re-issue.
    row.ladder_state = LadderState::Running(Rung::Respawn);
    row.phase = Phase::Killing;
    row.old_identity = Some(old.clone());
    write_recovery_row(state_dir, &row)?;

    // IDENTITY FENCE (1): re-confirm the live pid is STILL our recorded identity. A
    // recycled pid (start-time drifted beyond the kill-path slack) is a stranger —
    // ABORT without killing. A gone pid (None) proceeds: the kill is a no-op/ESRCH.
    if let Some(cur) = (io.current_start_ms)(old.pid) {
        if (cur - old.start_ms).abs() > crate::kill::START_TIME_SLACK_MS {
            row.phase = Phase::Idle;
            row.ladder_state = LadderState::Idle;
            write_recovery_row(state_dir, &row)?;
            return Ok(Rung4Outcome::AbortedIdentityMismatch);
        }
    }

    // SIGKILL the old identity (pidfd-bound, reuse-safe). ESRCH = already dead = ok.
    match (io.kill)(old.pid) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
        Err(e) => return Err(e),
    }

    // PHASE 2 — Tombstoned. IDENTITY FENCE (2): the CAS-guarded tombstone REFUSES a
    // reused-PID live row (never destroy a live successor, §3 P5.2).
    match registry::tombstone_guarded(sessions_dir, old.pid as i64, old_row_started_at) {
        TombstoneOutcome::Tombstoned | TombstoneOutcome::Absent => {}
        TombstoneOutcome::Refused { on_disk_started_at } => {
            row.phase = Phase::Idle;
            row.ladder_state = LadderState::Idle;
            write_recovery_row(state_dir, &row)?;
            return Ok(Rung4Outcome::AbortedReusedPidRow { on_disk_started_at });
        }
    }
    row.phase = Phase::Tombstoned;
    write_recovery_row(state_dir, &row)?;

    // PHASE 3 — Spawning. Mint a FRESH session_id (non-reuse) and RECORD it BEFORE the
    // spawn, so a crash after spawn finds the id and re-verifies the successor.
    let new_sid = (io.mint_session_id)()?;
    row.new_session_id = Some(new_sid.clone());
    row.phase = Phase::Spawning;
    write_recovery_row(state_dir, &row)?;
    let successor_pid = match (io.spawn_successor)(&new_sid) {
        Ok(pid) => pid,
        Err(_e) => {
            // Spawn failed — a confirmed failed recovery; leave the phase at Spawning
            // (a restart re-evaluates) and let the caller bump the strike counter.
            return Ok(Rung4Outcome::SpawnFailed {
                new_session_id: new_sid,
            });
        }
    };

    // PHASE 4 — Confirming. Poll for the successor `AliveReady` within the budget. On
    // confirm, clear the two-phase record (terminal, strikes reset). On timeout, leave
    // the phase at Confirming so a restart re-verifies (never blind re-kills).
    row.phase = Phase::Confirming;
    write_recovery_row(state_dir, &row)?;
    let deadline = Instant::now() + confirm_budget;
    loop {
        if (io.confirm_ready)(successor_pid, &new_sid) {
            row.phase = Phase::Idle;
            row.ladder_state = LadderState::Idle;
            row.consecutive_failures = 0;
            write_recovery_row(state_dir, &row)?;
            return Ok(Rung4Outcome::Recovered {
                new_session_id: new_sid,
                successor_pid,
            });
        }
        if Instant::now() >= deadline {
            return Ok(Rung4Outcome::SuccessorUnconfirmed {
                new_session_id: new_sid,
                successor_pid,
            });
        }
        std::thread::sleep(confirm_poll);
    }
}

/// The coordinator heartbeat path (`<state_dir>/recovery/coordinator.heartbeat`).
pub fn coordinator_heartbeat_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join("recovery")
        .join("coordinator.heartbeat")
}

/// Bump the coordinator heartbeat COUNTER on every epoll wake that actually processes
/// the ladder set (deepseek Gap-A: a COUNTER an external watcher verifies ADVANCES,
/// NOT mere process-existence — the "green heartbeat ≠ working" lesson). Atomic
/// tmp+rename.
pub fn bump_heartbeat(state_dir: &Path, counter: u64) -> io::Result<()> {
    let dir = state_dir.join("recovery");
    std::fs::create_dir_all(&dir)?;
    let path = coordinator_heartbeat_path(state_dir);
    let tmp = dir.join(format!(".coordinator.heartbeat.tmp.{}", std::process::id()));
    std::fs::write(&tmp, counter.to_string())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the heartbeat counter — the external watcher reads this and verifies it
/// ADVANCES between checks (a stalled-but-alive coordinator is caught and restarted).
pub fn read_heartbeat(state_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(coordinator_heartbeat_path(state_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

// ===========================================================================
// The coordinator's per-session evaluation — the FIRST production caller of
// authorizes_recovery (the carry-forward: classify_health had ZERO callers)
// ===========================================================================

/// One session's observations, the inputs the coordinator folds. signal-A and
/// signal-B arrive as TWO INDEPENDENT booleans (the non-vacuity constraint:
/// `evaluate_session` never derives one from the other) — exactly the contract
/// `classify_health` enforces. `signal_b_harness_live` is the safe-by-default cap
/// input: whether this harness wires a TRUSTED live signal-B producer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionObs {
    pub live: LifecycleState,
    pub status: SessionStatus,
    /// signal-A: the turn is past τ (`classify_obs` → `Stuck`).
    pub signal_a_stuck: bool,
    /// signal-B: output has gone silent past N (`progress_stale`).
    pub signal_b_stale: bool,
    /// Does this harness wire a TRUSTED live signal-B? (advisory-only ⇒ false ⇒ cap
    /// below Rung 4 — the Pete-binding safe-by-default cap.)
    pub signal_b_harness_live: bool,
    pub daemon_state: DaemonLiveness,
    pub is_headless: bool,
    pub connect_ok: Option<bool>,
    pub is_dead_dangling: bool,
    pub should_be_running: bool,
    pub now_ms: i64,
}

/// The coordinator's per-session decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderDecision {
    /// Healthy / non-actionable — no recovery.
    NoAction,
    /// Arm the ladder: enter at `entry`, never climbing past `ceiling` (the cap).
    Arm { entry: Rung, ceiling: Rung },
}

/// Fold a session's observations into a ladder decision — THE first production fold
/// of signal-A AND signal-B through [`classify_health`] into an arm/no-action verdict
/// (under the safe-by-default cap). This is the production caller of
/// [`Health::authorizes_recovery`] the carry-forward names (today ZERO callers); it
/// is PURE so the coordinator process stays a thin wrapper around it.
pub fn evaluate_session(obs: &SessionObs) -> LadderDecision {
    let health = classify_health(
        obs.live,
        obs.status,
        obs.signal_a_stuck,
        obs.signal_b_stale,
        obs.daemon_state,
        obs.is_headless,
        obs.now_ms,
    );
    let ceiling = ladder_ceiling(obs.signal_b_harness_live);
    match ladder_trigger(
        health,
        obs.connect_ok,
        obs.is_dead_dangling,
        obs.should_be_running,
    ) {
        Trigger::None => LadderDecision::NoAction,
        // A wedged-but-alive session climbs from Rung 1, capped at the ceiling.
        Trigger::Arm => LadderDecision::Arm {
            entry: Rung::PidfdSignal,
            ceiling,
        },
        // A dead-dangling sender enters at Rung 4 directly — but STILL capped (an
        // advisory-only harness holds it at Rung 3, never SIGKILL+respawn).
        Trigger::EnterAtRespawn => LadderDecision::Arm {
            entry: cap_rung(Rung::Respawn, obs.signal_b_harness_live),
            ceiling,
        },
    }
}

// ===========================================================================
// The coordinator control loop (one testable pass over the durable substrate)
// ===========================================================================

/// What one coordinator pass did over the recovery-row set (for logging + tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PassReport {
    /// The heartbeat counter AFTER this pass (the external watcher reads it advance).
    pub heartbeat: u64,
    pub rows_seen: usize,
    /// Rows whose EXPIRED lease this coordinator stole.
    pub leases_stolen: usize,
    /// Rows fenced out (a LIVE lease owned by a DIFFERENT coordinator) — untouched.
    pub fenced_out: usize,
    /// In-flight Rung-4 rows whose healthy successor was cleared (NOT re-killed).
    pub successors_cleared: usize,
    /// In-flight rows resumed (re-verify / resume-kill / re-arm).
    pub resumed: usize,
    /// Rows at CRIT (≥3 confirmed failures) — terminal, operator alert.
    pub crit: usize,
    /// Corrupt/unparseable rows treated as cold (skipped, not bricked).
    pub corrupt_skipped: usize,
}

/// Run ONE coordinator pass over `<state_dir>/recovery/*.json` — the testable core of
/// the standing `recovery_coordinator` process. It bumps the heartbeat COUNTER, and
/// for each durable row: honors the lease (steal-if-expired / fence-out a live foreign
/// owner), drives the crash-resume decision ([`resume_action`] — a healthy successor is
/// CLEARED, never re-killed), and tallies CRIT rows. Successor health is the cheap
/// flock liveness of the successor's own session_id (`is_live` via `probe_dead`); the
/// full incarnation re-verify is the ideal the durable fence enables.
///
/// This pass drives the DURABLE, crash-resilient, leased state machine (the genuinely
/// cross-process concern); the live-signal-driven ARMING is exercised in-process where
/// the signal producers live (the end-to-end live-chain gate).
pub fn coordinator_pass(
    state_dir: &Path,
    my_incarnation: u64,
    prev_heartbeat: u64,
    now_ms: i64,
) -> io::Result<PassReport> {
    let hb = prev_heartbeat.saturating_add(1);
    bump_heartbeat(state_dir, hb)?;
    let mut report = PassReport {
        heartbeat: hb,
        ..PassReport::default()
    };

    let recdir = state_dir.join("recovery");
    let entries = match std::fs::read_dir(&recdir) {
        Ok(e) => e,
        Err(_) => return Ok(report), // no recovery dir yet → nothing to do
    };
    for ent in entries.flatten() {
        let path = ent.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // Only session rows (skip the heartbeat + tmp files).
        let Some(session_id) = name.strip_suffix(".json") else {
            continue;
        };
        if session_id.starts_with('.') || session_id == "coordinator.heartbeat" {
            continue;
        }
        // Corrupt/unparseable → treated as cold (grok), never bricks recovery.
        let Some(mut row) = read_recovery_row(state_dir, session_id) else {
            report.corrupt_skipped += 1;
            continue;
        };
        report.rows_seen += 1;

        // Lease: fence out a LIVE lease owned by a DIFFERENT coordinator.
        if !can_acquire_lease(row.ladder_owner.as_ref(), my_incarnation, now_ms) {
            report.fenced_out += 1;
            continue;
        }
        // Steal an EXPIRED lease owned by someone else.
        if let Some(o) = &row.ladder_owner {
            if o.is_expired(now_ms) && o.coordinator_incarnation != my_incarnation {
                report.leases_stolen += 1;
            }
        }

        // CRIT is terminal — alert, no further automation. Emit the forensic
        // `recovery-crit` event ONCE, on the durable transition INTO CRIT (guarded
        // by `ladder_state` so a standing CRIT row is not re-logged every pass).
        // Best-effort (the durable row is the source of truth; the log is forensics).
        if row.consecutive_failures >= CRIT_CONSECUTIVE_FAILURES {
            report.crit += 1;
            if row.ladder_state != LadderState::Crit {
                let writer = crate::events::EventWriter::for_key(
                    state_dir,
                    session_id,
                    Some(session_id.to_string()),
                    None,
                );
                let _ = crate::events::emit_ladder_event(
                    &writer,
                    &crate::effects::FixedClock(now_ms),
                    &crate::events::LadderEvent::Crit {
                        session_id: session_id.to_string(),
                        consecutive_failures: row.consecutive_failures,
                    },
                );
                row.ladder_state = LadderState::Crit;
                let _ = write_recovery_row(state_dir, &row);
            }
            continue;
        }

        // Crash-resume: re-verify the successor by its OWN liveness (cheap flock).
        let successor_healthy = row
            .new_session_id
            .as_deref()
            .map(|nsid| !crate::livelock::probe_dead(state_dir, nsid))
            .unwrap_or(false);
        match resume_action(&row, successor_healthy) {
            ResumeAction::SuccessorHealthy => {
                // NEVER re-kill a healthy successor — clear the row's ladder.
                row.ladder_state = LadderState::Idle;
                row.phase = Phase::Idle;
                row.ladder_owner = None;
                let _ = write_recovery_row(state_dir, &row);
                report.successors_cleared += 1;
            }
            ResumeAction::ReArm(_)
            | ResumeAction::ReverifySuccessor { .. }
            | ResumeAction::ResumeKill { .. } => {
                report.resumed += 1;
            }
            ResumeAction::Cold => {}
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // --- Rungs --------------------------------------------------------------

    #[test]
    fn rung_levels_and_escalation_are_ordered() {
        assert_eq!(Rung::PidfdSignal.level(), 1);
        assert_eq!(Rung::Respawn.level(), 4);
        assert!(Rung::PidfdSignal < Rung::Respawn);
        assert_eq!(Rung::PidfdSignal.escalate(), Some(Rung::ControlWake));
        assert_eq!(Rung::Respawn.escalate(), None);
        assert!(Rung::Respawn.is_destructive());
        assert!(!Rung::PtyInject.is_destructive());
    }

    // --- Triggers -----------------------------------------------------------

    #[test]
    fn wedged_arms_from_rung_one() {
        assert_eq!(
            ladder_trigger(Health::Wedged, Some(true), false, true),
            Trigger::Arm
        );
    }

    #[test]
    fn busy_and_idle_never_arm() {
        for h in [Health::Busy, Health::Idle, Health::Dead] {
            assert_eq!(
                ladder_trigger(h, Some(true), false, true),
                Trigger::None,
                "{h:?} must not arm the ladder"
            );
        }
    }

    #[test]
    fn dead_dangling_enters_at_respawn_directly() {
        // Even a non-wedged health: a dead-dangling sender goes straight to Rung 4.
        assert_eq!(
            ladder_trigger(Health::Idle, None, true, true),
            Trigger::EnterAtRespawn
        );
    }

    #[test]
    fn channel_down_arms_only_when_should_be_running() {
        assert_eq!(
            ladder_trigger(Health::Idle, Some(false), false, true),
            Trigger::Arm
        );
        assert_eq!(
            ladder_trigger(Health::Idle, Some(false), false, false),
            Trigger::None,
            "a channel-down session that should NOT be running is not armed"
        );
    }

    // --- Safe-by-default cap ------------------------------------------------

    #[test]
    fn cap_permits_respawn_only_with_live_signal_b() {
        assert!(destructive_authorized(true));
        assert!(!destructive_authorized(false));
        assert_eq!(ladder_ceiling(true), Rung::Respawn);
        assert_eq!(ladder_ceiling(false), Rung::PtyInject);
    }

    #[test]
    fn advisory_only_harness_caps_below_rung_four() {
        // A desired Rung 4 on an advisory-only harness is held at Rung 3 — NEVER
        // SIGKILL+respawn (the Pete-binding cap).
        assert_eq!(cap_rung(Rung::Respawn, false), Rung::PtyInject);
        // With live signal-B the full ladder is permitted.
        assert_eq!(cap_rung(Rung::Respawn, true), Rung::Respawn);
        // The cap never PROMOTES a rung.
        assert_eq!(cap_rung(Rung::ControlWake, true), Rung::ControlWake);
    }

    // --- Throttle / back-off ------------------------------------------------

    const GIB: u64 = 1024 * 1024 * 1024;

    fn load(load1: f64, avail: u64, ncpu: usize, inflight: usize) -> HostLoad {
        HostLoad {
            loadavg_1m: load1,
            avail_mem_bytes: avail,
            ncpu,
            respawns_in_flight: inflight,
        }
    }

    #[test]
    fn respawn_admitted_with_headroom_and_free_semaphore() {
        assert_eq!(
            respawn_admission(&load(1.0, 8 * GIB, 8, 0), 2 * GIB, 2.0),
            RespawnAdmission::Admit
        );
    }

    #[test]
    fn respawn_suspended_when_semaphore_full() {
        // The semaphore is checked FIRST — ≤1 fleet-wide.
        assert_eq!(
            respawn_admission(&load(0.1, 8 * GIB, 8, RESPAWN_SEMAPHORE_MAX), 2 * GIB, 2.0),
            RespawnAdmission::SuspendConcurrency
        );
    }

    #[test]
    fn respawn_suspended_under_low_mem() {
        assert_eq!(
            respawn_admission(&load(0.1, 1 * GIB, 8, 0), 2 * GIB, 2.0),
            RespawnAdmission::SuspendLowMem
        );
    }

    #[test]
    fn respawn_suspended_under_high_load() {
        // load 20 on 8 cpus with a 2.0/cpu ceiling (16) → suspend.
        assert_eq!(
            respawn_admission(&load(20.0, 8 * GIB, 8, 0), 2 * GIB, 2.0),
            RespawnAdmission::SuspendHighLoad
        );
    }

    #[test]
    fn d_state_blocks_respawn_loop() {
        assert!(d_state_blocks_respawn('D'));
        for s in ['R', 'S', 'Z', 'I', 'T'] {
            assert!(!d_state_blocks_respawn(s), "{s} must not block");
        }
    }

    // --- Lease --------------------------------------------------------------

    #[test]
    fn expired_lease_is_stealable_live_lease_is_not() {
        let owner = LadderOwner::new(7, 1_000, 1_000); // expiry = 1000 + 96000 + 1000
        assert!(!owner.is_expired(1_000));
        assert!(owner.is_expired(owner.expiry_ms));

        // Another coordinator cannot acquire a LIVE lease it doesn't own.
        assert!(!can_acquire_lease(Some(&owner), /*me*/ 9, 1_000));
        // It CAN once expired (steal).
        assert!(can_acquire_lease(Some(&owner), 9, owner.expiry_ms));
        // The owning coordinator is re-entrant.
        assert!(can_acquire_lease(Some(&owner), 7, 1_000));
        // Unowned → free.
        assert!(can_acquire_lease(None, 9, 1_000));
    }

    // --- Durable row + corrupt-row resilience -------------------------------

    #[test]
    fn recovery_row_round_trips() {
        let dir = tempdir().unwrap();
        let mut row = RecoveryRow::cold("sess-xyz");
        row.ladder_state = LadderState::Running(Rung::ControlWake);
        row.phase = Phase::Confirming;
        row.old_identity = Some(Identity {
            pid: 4242,
            start_ms: 1_700_000_000_000,
            incarnation: 3,
        });
        row.new_session_id = Some("sess-successor".into());
        row.consecutive_failures = 2;
        write_recovery_row(dir.path(), &row).unwrap();
        let back = read_recovery_row(dir.path(), "sess-xyz").unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn corrupt_row_reads_as_cold_not_stuck() {
        let dir = tempdir().unwrap();
        let recdir = dir.path().join("recovery");
        std::fs::create_dir_all(&recdir).unwrap();
        std::fs::write(recdir.join("sess-bad.json"), b"{not valid json").unwrap();
        // Corrupt → None (cold), never a parse panic / permanent brick (grok).
        assert!(read_recovery_row(dir.path(), "sess-bad").is_none());
    }

    #[test]
    fn missing_row_reads_as_none() {
        let dir = tempdir().unwrap();
        assert!(read_recovery_row(dir.path(), "sess-absent").is_none());
    }

    // --- Crash-resume (P1.1) ------------------------------------------------

    #[test]
    fn healthy_successor_is_never_re_killed() {
        let mut row = RecoveryRow::cold("s");
        row.phase = Phase::Confirming;
        row.new_session_id = Some("succ".into());
        // The incarnation fence says the successor is alive → clear, do nothing.
        assert_eq!(resume_action(&row, true), ResumeAction::SuccessorHealthy);
    }

    #[test]
    fn unhealthy_post_mint_reverifies_never_blind_reissues() {
        let mut row = RecoveryRow::cold("s");
        row.phase = Phase::Spawning;
        row.new_session_id = Some("succ".into());
        assert_eq!(
            resume_action(&row, false),
            ResumeAction::ReverifySuccessor {
                new_session_id: "succ".into()
            }
        );
    }

    #[test]
    fn pre_mint_phase_resumes_kill_after_reeval() {
        let mut row = RecoveryRow::cold("s");
        row.phase = Phase::Killing;
        row.old_identity = Some(Identity {
            pid: 10,
            start_ms: 20,
            incarnation: 1,
        });
        match resume_action(&row, false) {
            ResumeAction::ResumeKill { old_identity } => {
                assert_eq!(old_identity.unwrap().pid, 10);
            }
            other => panic!("expected ResumeKill, got {other:?}"),
        }
    }

    #[test]
    fn non_destructive_rung_re_arms() {
        let mut row = RecoveryRow::cold("s");
        row.ladder_state = LadderState::Running(Rung::PtyInject);
        assert_eq!(resume_action(&row, false), ResumeAction::ReArm(Rung::PtyInject));
    }

    // --- Strike counter (RF-3) ----------------------------------------------

    #[test]
    fn strikes_increment_on_confirmed_failure_reach_crit_at_three() {
        let (c1, crit1) = apply_outcome(0, RecoveryOutcome::ConfirmedFailure);
        assert_eq!((c1, crit1), (1, false));
        let (c2, crit2) = apply_outcome(c1, RecoveryOutcome::ConfirmedFailure);
        assert_eq!((c2, crit2), (2, false));
        let (c3, crit3) = apply_outcome(c2, RecoveryOutcome::ConfirmedFailure);
        assert_eq!((c3, crit3), (3, true), "3 CONFIRMED failures → CRIT");
    }

    #[test]
    fn recovered_resets_strikes_indeterminate_leaves_them() {
        assert_eq!(apply_outcome(2, RecoveryOutcome::Recovered), (0, false));
        // P1.2: an indeterminate in-flight attempt must NOT touch the counter.
        assert_eq!(apply_outcome(2, RecoveryOutcome::Indeterminate), (2, false));
    }

    // --- evaluate_session (the production fold of signal-A AND signal-B) -----

    fn obs(
        live: LifecycleState,
        status: SessionStatus,
        a: bool,
        b: bool,
        harness_live: bool,
    ) -> SessionObs {
        SessionObs {
            live,
            status,
            signal_a_stuck: a,
            signal_b_stale: b,
            signal_b_harness_live: harness_live,
            daemon_state: DaemonLiveness::Up,
            is_headless: true,
            connect_ok: Some(true),
            is_dead_dangling: false,
            should_be_running: true,
            now_ms: 1_000_000,
        }
    }

    #[test]
    fn wedged_with_live_signal_b_arms_full_ladder() {
        // BOTH signals stale + a trusted live signal-B harness → Wedged → arm from
        // Rung 1, ceiling Rung 4.
        let d = evaluate_session(&obs(
            LifecycleState::AliveWorking,
            SessionStatus::Busy,
            true,
            true,
            true,
        ));
        assert_eq!(
            d,
            LadderDecision::Arm {
                entry: Rung::PidfdSignal,
                ceiling: Rung::Respawn
            }
        );
    }

    #[test]
    fn wedged_on_advisory_only_harness_is_capped_below_rung_four() {
        // Wedged, but NO trusted live signal-B (advisory-only) → ceiling Rung 3,
        // NEVER SIGKILL+respawn (the Pete-binding cap, through the real fold).
        let d = evaluate_session(&obs(
            LifecycleState::AliveWorking,
            SessionStatus::Busy,
            true,
            true,
            false,
        ));
        assert_eq!(
            d,
            LadderDecision::Arm {
                entry: Rung::PidfdSignal,
                ceiling: Rung::PtyInject
            }
        );
    }

    #[test]
    fn streaming_longturn_is_no_action_through_the_real_fold() {
        // RF-1, end-to-end through evaluate_session: signal-A fired (past τ) but
        // signal-B FRESH (streaming) → classify_health = Busy → NO ladder. This is
        // the keystone false-positive control on the production caller.
        let d = evaluate_session(&obs(
            LifecycleState::Stuck,
            SessionStatus::Busy,
            true,
            /* signal_b_stale */ false,
            true,
        ));
        assert_eq!(d, LadderDecision::NoAction, "a healthy streaming long turn must not arm");
    }

    // --- Rung IO seam -------------------------------------------------------

    #[test]
    fn rung1_pidfd_signals_a_live_process() {
        // SIGCONT to OURSELVES (a live process) is harmless and must succeed — the
        // open-then-send path works against a real, alive pid.
        let me = std::process::id() as i32;
        rung1_pidfd_signal(me, RUNG1_SIGNAL).expect("pidfd SIGCONT to self");
    }

    #[test]
    fn rung1_pidfd_errors_on_a_dead_pid() {
        // A pid that does not exist → pidfd_open ESRCH → Err (the already-dead signal
        // Rung 1 reads as "no point climbing — the target is gone").
        let err = rung1_pidfd_signal(0x7FFF_FFF0, RUNG1_SIGNAL)
            .expect_err("a nonexistent pid must error");
        let _ = err; // ESRCH (or EINVAL on some kernels) — either way, not Ok.
    }

    #[test]
    fn coordinator_pass_heartbeats_clears_healthy_successor_and_flags_crit() {
        let dir = tempdir().unwrap();

        // (1) An in-flight Rung-4 row whose successor is healthy → must be CLEARED
        // (never re-killed). new_session_id="succ" has no flock file → fail-closed
        // alive → successor_healthy.
        let mut healthy = RecoveryRow::cold("sess-healthy");
        healthy.phase = Phase::Confirming;
        healthy.ladder_state = LadderState::Running(Rung::Respawn);
        healthy.new_session_id = Some("succ".into());
        write_recovery_row(dir.path(), &healthy).unwrap();

        // (2) A CRIT row (3 confirmed failures) → flagged, no further automation.
        let mut crit = RecoveryRow::cold("sess-crit");
        crit.consecutive_failures = 3;
        write_recovery_row(dir.path(), &crit).unwrap();

        // (3) A corrupt row → treated as cold (skipped, never bricks).
        std::fs::write(
            dir.path().join("recovery").join("sess-bad.json"),
            b"{garbage",
        )
        .unwrap();

        let report = coordinator_pass(dir.path(), /*my_incarnation*/ 5, /*prev_hb*/ 0, 1_000)
            .expect("pass");

        assert_eq!(report.heartbeat, 1, "heartbeat advanced");
        assert_eq!(report.successors_cleared, 1, "healthy successor cleared, not re-killed");
        assert_eq!(report.crit, 1, "CRIT row flagged");
        assert_eq!(report.corrupt_skipped, 1, "corrupt row treated as cold");

        // The healthy-successor row's ladder is now Idle (cleared) — not re-killed.
        let back = read_recovery_row(dir.path(), "sess-healthy").unwrap();
        assert_eq!(back.ladder_state, LadderState::Idle);
        assert_eq!(back.phase, Phase::Idle);
    }

    #[test]
    fn coordinator_pass_fences_out_a_live_foreign_lease() {
        let dir = tempdir().unwrap();
        // A row leased LIVE by coordinator incarnation 9 (not me).
        let mut row = RecoveryRow::cold("sess-leased");
        row.ladder_owner = Some(LadderOwner::new(9, 1_000, 1_000)); // expiry well in the future
        row.phase = Phase::Killing;
        write_recovery_row(dir.path(), &row).unwrap();

        let report = coordinator_pass(dir.path(), /*me*/ 5, 0, 1_000).expect("pass");
        assert_eq!(report.fenced_out, 1, "a live foreign lease fences me out");
        assert_eq!(report.resumed, 0, "I must not touch a row I am fenced out of");
    }

    #[test]
    fn heartbeat_counter_round_trips_and_is_monotonic() {
        let dir = tempdir().unwrap();
        assert_eq!(read_heartbeat(dir.path()), None, "absent → None");
        bump_heartbeat(dir.path(), 1).unwrap();
        assert_eq!(read_heartbeat(dir.path()), Some(1));
        bump_heartbeat(dir.path(), 42).unwrap();
        assert_eq!(read_heartbeat(dir.path()), Some(42), "watcher sees the advance");
    }

    #[test]
    fn dead_dangling_enters_at_respawn_but_cap_still_applies() {
        // dead-dangling + live signal-B → enter at Rung 4.
        let mut o = obs(LifecycleState::Gone, SessionStatus::Idle, false, false, true);
        o.is_dead_dangling = true;
        assert_eq!(
            evaluate_session(&o),
            LadderDecision::Arm {
                entry: Rung::Respawn,
                ceiling: Rung::Respawn
            }
        );
        // dead-dangling + advisory-only → entry CAPPED at Rung 3 (no SIGKILL+respawn).
        o.signal_b_harness_live = false;
        assert_eq!(
            evaluate_session(&o),
            LadderDecision::Arm {
                entry: Rung::PtyInject,
                ceiling: Rung::PtyInject
            }
        );
    }

    // --- Rung 4 destructive executor (deterministic, fake seams) -------------

    fn busy_row(sessions_dir: &Path, pid: i64, started_at: i64) {
        let entry = crate::registry::RegistryEntry {
            pid: Some(pid),
            session_id: Some(format!("sess-{pid}")),
            started_at: Some(started_at),
            updated_at: Some(started_at),
            status: Some("busy".into()),
            name: Some(format!("n-{pid}")),
            ..Default::default()
        };
        crate::registry::write_entry(sessions_dir, &entry).unwrap();
    }

    /// Happy path: identity matches → SIGKILL → CAS-tombstone → mint fresh → spawn →
    /// confirm AliveReady → Recovered. The durable row ends at the terminal Idle phase
    /// and the old row is tombstoned.
    #[test]
    fn execute_rung4_happy_path_two_phase_recovered() {
        let dir = tempdir().unwrap();
        let sd = dir.path();
        let (pid, started) = (31001_i32, 1_000_000_i64);
        busy_row(sd, pid as i64, started);
        let old = Identity { pid, start_ms: started, incarnation: 1 };
        let killed = std::cell::Cell::new(false);
        let io = Rung4Io {
            kill: &|_p| {
                killed.set(true);
                Ok(())
            },
            current_start_ms: &|_p| Some(started), // identity matches
            mint_session_id: &|| Ok("fresh-sid".to_string()),
            spawn_successor: &|_s| Ok(42),
            confirm_ready: &|_p, _s| true,
        };
        let out = execute_rung4(
            sd,
            sd,
            RecoveryRow::cold("s"),
            &old,
            Some(started),
            Duration::from_millis(200),
            Duration::from_millis(5),
            &io,
        )
        .unwrap();
        assert_eq!(
            out,
            Rung4Outcome::Recovered {
                new_session_id: "fresh-sid".into(),
                successor_pid: 42
            }
        );
        assert!(killed.get(), "the old identity was SIGKILLed");
        assert_eq!(
            read_recovery_row(sd, "s").unwrap().phase,
            Phase::Idle,
            "the two-phase record is cleared to the terminal Idle on confirm"
        );
        assert!(
            crate::registry::read_entry(sd, pid as i64).is_none(),
            "the old row is CAS-tombstoned"
        );
    }

    /// The CAS-guarded tombstone REFUSES a reused-PID live row → AbortedReusedPidRow,
    /// the live successor row survives, and no mint/spawn happens (§3 P5.2 fence inside
    /// the live destructive path).
    #[test]
    fn execute_rung4_cas_refuses_reused_pid_row_aborts() {
        let dir = tempdir().unwrap();
        let sd = dir.path();
        let (pid, recorded, reused) = (31002_i32, 1_000_000_i64, 2_000_000_i64);
        busy_row(sd, pid as i64, reused); // a NEW incarnation reused the PID's row
        let old = Identity { pid, start_ms: recorded, incarnation: 1 };
        let io = Rung4Io {
            kill: &|_p| Ok(()),
            current_start_ms: &|_p| Some(recorded),
            mint_session_id: &|| panic!("must not mint after a refused tombstone"),
            spawn_successor: &|_s| panic!("must not spawn"),
            confirm_ready: &|_p, _s| panic!("must not confirm"),
        };
        let out = execute_rung4(
            sd,
            sd,
            RecoveryRow::cold("s"),
            &old,
            Some(recorded),
            Duration::from_millis(50),
            Duration::from_millis(5),
            &io,
        )
        .unwrap();
        assert_eq!(
            out,
            Rung4Outcome::AbortedReusedPidRow {
                on_disk_started_at: Some(reused)
            }
        );
        assert!(
            crate::registry::read_entry(sd, pid as i64).is_some(),
            "the reused-PID live successor row survives the refused tombstone"
        );
    }

    /// A successor that never reaches AliveReady within the budget → SuccessorUnconfirmed,
    /// and the durable phase stays Confirming so a restart re-verifies (never blind
    /// re-kills — the P1.1 contract).
    #[test]
    fn execute_rung4_unconfirmed_successor_leaves_phase_confirming() {
        let dir = tempdir().unwrap();
        let sd = dir.path();
        let (pid, started) = (31003_i32, 1_000_000_i64);
        busy_row(sd, pid as i64, started);
        let old = Identity { pid, start_ms: started, incarnation: 1 };
        let io = Rung4Io {
            kill: &|_p| Ok(()),
            current_start_ms: &|_p| Some(started),
            mint_session_id: &|| Ok("fresh".to_string()),
            spawn_successor: &|_s| Ok(99),
            confirm_ready: &|_p, _s| false, // never AliveReady
        };
        let out = execute_rung4(
            sd,
            sd,
            RecoveryRow::cold("s"),
            &old,
            Some(started),
            Duration::from_millis(30),
            Duration::from_millis(5),
            &io,
        )
        .unwrap();
        assert_eq!(
            out,
            Rung4Outcome::SuccessorUnconfirmed {
                new_session_id: "fresh".into(),
                successor_pid: 99
            }
        );
        assert_eq!(
            read_recovery_row(sd, "s").unwrap().phase,
            Phase::Confirming,
            "the phase stays Confirming so a restart re-verifies, never blind re-kills"
        );
    }
}
