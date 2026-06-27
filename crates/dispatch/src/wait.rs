//! `qd wait` poll loop — STATUS + TRANSCRIPT-CONTENT keyed (wart-wave W6+W7,
//! ADD-15), PURE over injected deps.
//!
//! HISTORY: the A4 port (status.ts:359-390) was status-keyed ONLY, with an
//! explicit anti-"fix" note ("qd wait stays STATUS-keyed") guarding against an
//! UNSANCTIONED semantic fork. Pete sanctioned the fork 2026-06-05 19:47 EDT
//! (ADD-15 W7: continuity WAIVED; W6: close the busy-poll window where cheap) —
//! the note retired WITH that ruling, carrier: coverage-matrix wait row +
//! exec/divergence-table.md W6+W7 row.
//!
//! THE W6 WINDOW (why content keying exists): a turn that starts AND ends inside
//! one 500ms poll interval never latches `saw_busy`, so the status-keyed loop
//! polled idle to TIMEOUT on a turn that actually completed. The turn's JSONL
//! assistant record is DURABLE — keying completion evidence on it closes the
//! window without rebuilding the poller (the brief's "narrow the gap" bound).
//!
//! Done = status `idle` AND (`saw_busy` OR `saw_turn_end`). Status stays the
//! COMPLETION signal; content is an additional EVIDENCE source — the same
//! posture as send:pty --wait's decide_wait (idle completes, records attribute).
//! Turn-end evidence = an assistant record with NESTED `message.stop_reason ==
//! "end_turn"` (exactly the field `extract_response` reads, sendpty.rs — NOT a
//! top-level `stop_reason`, which never exists and would build a dead latch)
//! appearing PAST the entry-time transcript offset.
//!
//! DEGRADATION (works-well, D4 warn-not-fail precedent): transcript unresolvable
//! at entry, or integrity lost mid-wait (file shrank past the offset / offset
//! off a char boundary) → ONE stderr warn, the loop continues STATUS-ONLY
//! (today's exact pre-fix behavior). Re-reading a shrunk file from byte 0 could
//! latch an OLD end_turn record → false done — the same wrong-anchor class the
//! send:pty loop fails loud on; here the ADDITIVE key degrades instead, because
//! status-keying alone is the complete pre-fix contract.
//!
//! IDLE-WITHOUT-EVIDENCE rule (was labeled "W5 invariant" — that meant a4-spec
//! W5, RENAMED to kill the collision with the wart-sweep's unrelated W5): an
//! in-loop `idle` with NO evidence (no `saw_busy`, no `saw_turn_end`) does NOT
//! exit — it keeps polling to the timeout. (status.ts:373-383 shape preserved;
//! the evidence SET widened under the sanction.)
//!
//! The OpenCode SSE branch (status.ts:225-358) stays a NAMED PARKED EXCLUSION
//! (provider is never opencode in the engine).

use crate::boot::{read_pid_status, Sleeper};
use crate::effects::Clock;
use crate::sendpty::parse_jsonl_slice;
use std::cell::Cell;
use std::path::PathBuf;

/// §6.4 thin interceptor for bug #3 (engsol S-1). The ONE swap-point the wait
/// loop's turn-completion EVIDENCE flows through, so the parallel (B) track can
/// replace the IMPLEMENTATION without touching the call site (`bin/dispatch/verbs/
/// wait.rs::run_wait`) or the poll loop.
///
/// Today's impl is the transcript page-cache **VISIBILITY barrier**
/// ([`RealTurnEndProbe`]): the EXPECTED turn's `end_turn` record becoming
/// VISIBLE in the JSONL past the entry-time anchor — page-cache visibility, **not
/// fsync durability** (the consumer can't force durability and doesn't need it
/// for coordination; memo #3 §6.1.2/.3, *superseded-if-(B)*). (B) swaps this for
/// the stream-json `result` event (the §6.0-pure answer) by constructing a
/// different `TurnCompletion` at the one call site — the loop is unchanged.
pub trait TurnCompletion {
    /// Probe whether the expected-turn completion marker is VISIBLE yet. Pure
    /// observation — no waiting; the loop owns the poll cadence + deadline.
    fn poll_completion(&self) -> TurnCompletionProbe;
}

/// The outcome of one [`TurnCompletion::poll_completion`] — the three states the
/// memo's §6.1.6 distinguishes (turn-complete-visible / in-progress /
/// transcript-unresolvable→degrade; the loop adds timeout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnCompletionProbe {
    /// The expected-turn end marker is VISIBLE past the entry anchor → latch done.
    Visible,
    /// Not visible yet → keep polling (the idle-without-evidence rule: no false
    /// done on a quiet poll).
    Pending,
    /// Source integrity lost (file shrank below the anchor / offset off a char
    /// boundary / otherwise unresolvable mid-wait) → degrade to status-only and
    /// warn ONCE; never re-scan from byte 0 (that could latch an OLD end_turn).
    Degraded(String),
}

/// Effects the loop needs, injected so the latches + the idle-without-evidence
/// rule are unit-testable without real fs/clock/sleep.
pub trait WaitContentDeps {
    /// Read the session status from the pid file. `None` → unreadable/parse error
    /// (TS `catch` → "session exited", status.ts:384-387).
    fn read_status(&self) -> Option<String>;
    /// Probe the transcript for turn-end evidence past the entry offset.
    /// `Ok(true)`  → an assistant `message.stop_reason == "end_turn"` record
    ///               appeared past the offset (latches; not re-polled after).
    /// `Ok(false)` → not yet (keep polling).
    /// `Err(why)`  → source integrity lost (shrank/boundary) → the loop warns
    ///               ONCE and degrades to status-only (see module doc).
    fn poll_turn_end(&self) -> Result<bool, String>;
    /// Emit a single degradation warning (stderr in the real wiring).
    fn warn(&self, msg: &str);
    /// Sleep `ms` (the 500ms poll; seamed so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic-ish clock in ms (the timeout deadline).
    fn now_ms(&self) -> i64;
}

/// Terminal outcome of the `qd wait` loop. The bin maps each to its stderr
/// suffix + exit code (UNCHANGED surface: " done"/" session exited"/" timeout").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatusOutcome {
    /// `idle` observed WITH evidence (saw_busy OR saw_turn_end) → " done", exit 0.
    Done,
    /// The status became unreadable mid-wait → " session exited", exit 1
    /// (status.ts:384-387).
    SessionExited,
    /// The deadline elapsed (incl. idle-without-evidence polled to timeout) →
    /// " timeout", exit 1 (status.ts:389-390).
    Timeout,
}

/// The `qd wait` poll loop, PURE over [`WaitContentDeps`] (W6+W7 shape; see
/// module doc for the contract and its sanction).
///
/// Each iteration (while `now - start < timeout_ms`): read status (`None` →
/// [`WaitStatusOutcome::SessionExited`]); `busy`/`shell` latches `saw_busy`;
/// the content probe (until latched or degraded) may latch `saw_turn_end`;
/// `idle` AND any evidence → [`WaitStatusOutcome::Done`]; else sleep `poll_ms`.
/// Deadline → [`WaitStatusOutcome::Timeout`].
pub fn run_wait_content_loop(
    deps: &dyn WaitContentDeps,
    timeout_ms: i64,
    poll_ms: u64,
) -> WaitStatusOutcome {
    let start = deps.now_ms();
    let mut saw_busy = false;
    let mut saw_turn_end = false;
    let mut content_alive = true;

    while deps.now_ms() - start < timeout_ms {
        let Some(status) = deps.read_status() else {
            return WaitStatusOutcome::SessionExited;
        };

        if status == "busy" || status == "shell" {
            saw_busy = true;
        }

        // Content probe: consult until evidence latches or the source degrades.
        if content_alive && !saw_turn_end {
            match deps.poll_turn_end() {
                Ok(true) => saw_turn_end = true,
                Ok(false) => {}
                Err(why) => {
                    deps.warn(&format!(
                        "qd wait: transcript unavailable mid-wait ({why}) — continuing status-keyed only"
                    ));
                    content_alive = false;
                }
            }
        }

        // Idle WITH evidence = the turn finished → done. Idle WITHOUT evidence
        // keeps polling (idle-without-evidence rule, module doc). Any other
        // status (cold/killed/unknown) likewise keeps polling.
        if status == "idle" && (saw_busy || saw_turn_end) {
            return WaitStatusOutcome::Done;
        }

        deps.sleep(poll_ms);
    }

    WaitStatusOutcome::Timeout
}

// ===========================================================================
// WP-B3 (§6.0-purity keystone, R1 / §H.2 / gate D2) — route turn-completion +
// control-status through the daemon CHANNEL, NOT a disk read.
// ===========================================================================
//
// The §6.0 root-cause invariant: NO control decision on the healthy real-time
// path may rest on a persistence/replay artifact (the `pid.json` status file /
// the transcript). Two channel seams re-source the loop's inputs; the disk reads
// survive ONLY as the explicit channel-DOWN fallback:
//   - [`ChannelTurnSource`] backs the channel-keyed [`ChannelTurnCompletion`]
//     (item 1) — the EXPECTED turn's republished `result` event is the
//     completion evidence; the (A) [`RealTurnEndProbe`] transcript barrier is the
//     channel-down fallback.
//   - [`ChannelStatusSource`] re-sources [`WaitContentDeps::read_status`] (R1-a) —
//     the daemon-written real-time status; the `pid.json` read ([`StatusFallback`])
//     fires ONLY when the channel is Down.
//
// In production B2b-2b wires ONE socket subscriber behind BOTH seams, so
// "channel-down" is a SINGLE shared truth (the two seams cannot disagree). Until
// that fold, the bin constructs `status_source = None` + `completion =
// RealTurnEndProbe` = honest channel-down mode (today's exact behavior). The
// §H.2 purity test (below) proves the HEALTHY path reads no `pid.json`.

/// Channel seam #1 (item 1): the daemon-republished turn-completion fact for the
/// EXPECTED turn (matched by `session_id` + turn identity INSIDE the real impl;
/// the seam only reports the three states). A fake drives unit tests without a
/// live socket; the real socket-subscriber folds in at B2b-2b/B7.
pub trait ChannelTurnSource {
    fn poll_result(&self) -> ChannelTurnObservation;
}

/// The three states [`ChannelTurnSource::poll_result`] reports — mapped onto the
/// §6.4 [`TurnCompletionProbe`] by [`ChannelTurnCompletion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelTurnObservation {
    /// The expected turn's `result` event has been republished → `Visible`.
    Seen,
    /// Channel healthy, the `result` not republished yet → `Pending` (no false
    /// done on a quiet poll).
    NotYet,
    /// The channel is DOWN (subscriber never connected / dropped) → delegate to
    /// the (A) transcript-barrier fallback (`Degraded`/status-only — never a
    /// false done, never a hang).
    Down,
}

/// The channel-keyed [`TurnCompletion`] (item 1): completion evidence comes from
/// the daemon `result` event over [`ChannelTurnSource`]; when the channel is Down
/// it delegates to `fallback` (the (A) [`RealTurnEndProbe`] transcript barrier).
/// Constructed at the `bin/dispatch/verbs/wait.rs:131` swap site (in B2b-2b — until then
/// the bin uses the bare `RealTurnEndProbe` = channel-down mode).
pub struct ChannelTurnCompletion {
    pub source: Box<dyn ChannelTurnSource>,
    /// The channel-DOWN fallback: the (A) transcript visibility barrier.
    pub fallback: Box<dyn TurnCompletion>,
}

impl TurnCompletion for ChannelTurnCompletion {
    fn poll_completion(&self) -> TurnCompletionProbe {
        match self.source.poll_result() {
            ChannelTurnObservation::Seen => TurnCompletionProbe::Visible,
            ChannelTurnObservation::NotYet => TurnCompletionProbe::Pending,
            // Channel down → the (A) transcript barrier (its own Visible/Pending/
            // Degraded). The healthy `result` source is bypassed; this IS the
            // channel-down mode.
            ChannelTurnObservation::Down => self.fallback.poll_completion(),
        }
    }
}

/// The control status the daemon republishes over the channel (Ready→`Busy`,
/// `result`/TurnEnd→`Idle`, Breaker/Eof-abort→`Offline`). The loop reads the
/// string form ([`Self::as_str`]) so its `busy`/`idle` gates are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatus {
    Busy,
    Idle,
    Offline,
}

impl ChannelStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelStatus::Busy => "busy",
            ChannelStatus::Idle => "idle",
            ChannelStatus::Offline => "offline",
        }
    }
}

/// What [`ChannelStatusSource::poll_status`] reports this poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatusObservation {
    /// The channel is healthy; the latest republished control status.
    Live(ChannelStatus),
    /// The channel is DOWN → fall back to the [`StatusFallback`] disk read.
    Down,
}

/// Channel seam #2 (R1-a): the daemon-written real-time control status. A fake
/// drives unit tests; the real socket-subscriber folds in at B2b-2b/B7 (the SAME
/// subscriber that backs [`ChannelTurnSource`], so "channel-down" is one truth).
pub trait ChannelStatusSource {
    fn poll_status(&self) -> ChannelStatusObservation;
}

/// RESIDUAL #1 (BINDING, supervisor-ruled): resolve the PRE-LOOP entry-idle control
/// gate (`qd wait` short-circuits "is idle" / exit 0 when the session is already
/// idle at entry — `bin/dispatch/verbs/wait.rs`). On the HEALTHY channel (`Live`) the
/// answer is the daemon-written control status; the disk-resolved status
/// (`disk_idle`) is the channel-DOWN fallback ONLY. This EXTENDS the §6.0 invariant
/// — no control decision rests on a disk-sourced status outside explicit
/// channel-down mode — to the pre-loop gate B3's loop-purity test does NOT cover
/// (the loop only starts AFTER this gate). `disk_idle` is a lazy closure so the
/// §H.2 entry-idle purity test can prove it is NEVER consulted on the healthy path.
///
/// `Live(Idle)` → already idle (short-circuit). `Live(Busy|Offline)` → NOT
/// entry-idle (proceed to the wait loop, which owns the busy→idle / offline
/// terminal). `Down` (channel not up: non-headless session / no daemon / the
/// subscriber never connected) → the disk fallback answers — sanctioned
/// channel-down mode, today's exact behavior.
pub fn entry_is_idle(channel: ChannelStatusObservation, disk_idle: impl FnOnce() -> bool) -> bool {
    match channel {
        ChannelStatusObservation::Live(status) => matches!(status, ChannelStatus::Idle),
        ChannelStatusObservation::Down => disk_idle(),
    }
}

/// The channel-DOWN status fallback (R1): the legacy `pid.json` disk read, behind
/// a seam so the §H.2 purity test can INSTRUMENT it and assert it is NEVER called
/// on the healthy channel path (and IS called only when the channel is Down).
pub trait StatusFallback {
    fn read_disk_status(&self) -> Option<String>;
}

/// The production [`StatusFallback`]: read the `<pid>.json` status from disk. This
/// is a persistence artifact — it is the channel-DOWN fallback ONLY, never the
/// healthy-path control source (the §6.0 invariant).
pub struct PidFileStatus {
    pub pid_file: PathBuf,
}

impl StatusFallback for PidFileStatus {
    fn read_disk_status(&self) -> Option<String> {
        read_pid_status(&self.pid_file)
    }
}

/// The real (channel + fs/clock-backed) [`WaitContentDeps`].
///
/// `completion` is the §6.4 [`TurnCompletion`] interceptor: in B2b-2b it is the
/// channel-keyed [`ChannelTurnCompletion`]; until then the bare (A)
/// [`RealTurnEndProbe`] visibility barrier (channel-down mode). `None` when the
/// transcript was unresolvable at entry (the bin emits the entry-time warn).
///
/// `status_source` (R1-a) re-sources `read_status` through the daemon channel on
/// the HEALTHY path. `None` (today) or `Down` → `status_fallback` reads `pid.json`
/// — the channel-down fallback. No control decision rests on a disk-sourced status
/// outside that mode (the §6.0 invariant; proven by the §H.2 purity test).
pub struct RealWaitContentDeps<'a> {
    /// Healthy-path control status (R1-a). `None` until B2b-2b wires the real
    /// subscriber → channel-down mode (`status_fallback`).
    pub status_source: Option<Box<dyn ChannelStatusSource>>,
    /// Channel-DOWN status read (`pid.json`). Seamed so the §H.2 purity test can
    /// assert it is never reached on the healthy path.
    pub status_fallback: Box<dyn StatusFallback>,
    pub completion: Option<Box<dyn TurnCompletion>>,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
}

/// The fs-backed turn-end probe: re-reads the transcript slice past the entry
/// offset each poll (the same read shape as send.rs's --wait deps) and scans
/// for an assistant `message.stop_reason == "end_turn"` record.
pub struct RealTurnEndProbe {
    pub jsonl_path: PathBuf,
    /// Transcript byte length at loop entry (file absent at entry → 0).
    pub entry_offset: u64,
    /// Cheap skip: bytes already scanned w/o evidence (monotonic high-water).
    scanned: Cell<u64>,
}

impl RealTurnEndProbe {
    pub fn new(jsonl_path: PathBuf, entry_offset: u64) -> Self {
        Self {
            jsonl_path,
            entry_offset,
            scanned: Cell::new(0),
        }
    }

    fn poll(&self) -> Result<bool, String> {
        let content = match std::fs::read_to_string(&self.jsonl_path) {
            Ok(c) => c,
            // Absent/unreadable is NOT integrity loss for an ADDITIVE probe —
            // the transcript may simply not exist yet. Keep polling.
            Err(_) => return Ok(false),
        };
        let off = self.entry_offset as usize;
        if content.len() < off {
            // Shrank past the entry offset (truncated/rotated): re-scanning from
            // byte 0 could latch an OLD end_turn → degrade, never false-done.
            return Err(format!(
                "transcript shrank below the entry offset ({} < {off} bytes)",
                content.len()
            ));
        }
        let Some(slice) = content.get(off..) else {
            // Offset off a char boundary (entry snapshot fell mid-write).
            return Err(format!("entry offset {off} is not a char boundary"));
        };
        // Monotonic high-water skip: nothing new since last scan → no re-parse.
        if (content.len() as u64) <= self.scanned.get() {
            return Ok(false);
        }
        self.scanned.set(content.len() as u64);
        Ok(slice_has_turn_end(slice))
    }
}

/// [`RealTurnEndProbe`] IS the today's-impl [`TurnCompletion`] (the transcript
/// visibility barrier). The `Ok(true)/Ok(false)/Err` of [`Self::poll`] maps onto
/// the interceptor's `Visible/Pending/Degraded` states; (B) provides a different
/// `TurnCompletion` (stream-json `result`) at the call site without touching this.
impl TurnCompletion for RealTurnEndProbe {
    fn poll_completion(&self) -> TurnCompletionProbe {
        match self.poll() {
            Ok(true) => TurnCompletionProbe::Visible,
            Ok(false) => TurnCompletionProbe::Pending,
            Err(why) => TurnCompletionProbe::Degraded(why),
        }
    }
}

/// Does this JSONL slice contain an assistant record with NESTED
/// `message.stop_reason == "end_turn"`? (The exact field `extract_response`
/// reads — sendpty.rs; a top-level `stop_reason` does not exist in the
/// transcript shape and would never latch.)
pub fn slice_has_turn_end(slice: &str) -> bool {
    parse_jsonl_slice(slice).iter().any(|p| {
        p.value.get("type").and_then(|v| v.as_str()) == Some("assistant")
            && p.value
                .get("message")
                .and_then(|m| m.get("stop_reason"))
                .and_then(|v| v.as_str())
                == Some("end_turn")
    })
}

impl WaitContentDeps for RealWaitContentDeps<'_> {
    fn read_status(&self) -> Option<String> {
        // R1-a: on the HEALTHY path the control status comes from the daemon
        // CHANNEL, never a disk read. The `pid.json` read (`status_fallback`)
        // fires ONLY when the channel is Down (or not yet wired) — the §6.0
        // invariant the §H.2 purity test pins.
        if let Some(src) = &self.status_source {
            match src.poll_status() {
                // R-ii: map onto the FROZEN loop's exact string contract.
                // Busy/Idle drive `saw_busy` + the idle-with-evidence Done gate
                // exactly as the pid.json strings did.
                ChannelStatusObservation::Live(ChannelStatus::Busy) => {
                    return Some(ChannelStatus::Busy.as_str().to_string())
                }
                ChannelStatusObservation::Live(ChannelStatus::Idle) => {
                    return Some(ChannelStatus::Idle.as_str().to_string())
                }
                // Offline = the daemon republished Breaker/Eof-abort: the producer
                // died / the turn aborted. The real-time analog of today's
                // status-unreadable → `None` → `SessionExited` (exit 1, " session
                // exited"): fail-closed, never a hang, never a false Done. (B5 owns
                // the richer offline-vs-daemon-down taxonomy; B3 maps it onto the
                // loop's existing terminal.)
                ChannelStatusObservation::Live(ChannelStatus::Offline) => return None,
                // Channel down → fall through to the disk fallback.
                ChannelStatusObservation::Down => {}
            }
        }
        self.status_fallback.read_disk_status()
    }
    fn poll_turn_end(&self) -> Result<bool, String> {
        // Route the loop's content-evidence question through the §6.4
        // interceptor, mapping its three states onto the loop's seam contract
        // (`Ok(true)` latch / `Ok(false)` keep-polling / `Err` degrade-once).
        match &self.completion {
            Some(c) => match c.poll_completion() {
                TurnCompletionProbe::Visible => Ok(true),
                TurnCompletionProbe::Pending => Ok(false),
                TurnCompletionProbe::Degraded(why) => Err(why),
            },
            None => Ok(false), // entry-time degrade; bin already warned.
        }
    }
    fn warn(&self, msg: &str) {
        eprintln!("{msg}");
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

// ===========================================================================
// codex P2 W6 (codex-p2-spec section 7.6 wait paragraph) — the codex wait loop.
// ===========================================================================
//
// A codex session is a daemon-hosted protocol thread, not a mux pane with a pid
// file + a claude transcript — so it has its OWN wait loop. `qd wait <codex-row>`
// blocks until the thread goes IDLE. The status comes from one of two channels,
// abstracted behind [`CodexWaitDeps::poll_status`]:
//
//   - LIVE (preferred): a fresh ws client observes `thread/status/changed`
//     broadcasts (no subscriber role needed — Q1) and maps the notification via
//     `CodexProvider::parse_status` until the IDLE transition.
//   - FALLBACK (daemon unreachable): the durable rollout tail is polled and
//     `provider::codex::rollout::derive_status` derives Idle/Busy — the SAME
//     connectionless path W5's ls uses, so `wait` still resolves if the daemon is
//     gone (codex-p2-spec section 7.6).
//
// The real deps switch channels internally; the loop stays pure (Idle ⇒ done,
// anything else ⇒ keep polling to the timeout). The existing wait timeout knob is
// honored (the verb passes the same `timeout_ms`).

/// What one wait poll observed (codex-p2-spec section 7.6). The real deps produce
/// it from the live notification channel, falling back to the rollout tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexObservation {
    /// The thread is idle (the turn finished) → the loop completes.
    Idle,
    /// The thread is still active / busy → keep polling.
    Busy,
    /// No determination this poll (a quiet socket, an unparsed frame, an
    /// unreadable tail) → keep polling until the deadline.
    Pending,
}

/// Effects the codex wait loop needs (codex-p2-spec section 7.6), injected so the
/// live/fallback channel switch + the deadline are unit-testable without a real
/// socket/clock/sleep.
pub trait CodexWaitDeps {
    /// Observe the thread status for this poll. The real impl reads the next
    /// `thread/status/changed` notification (mapped via `parse_status`); on a
    /// connection drop it falls back to deriving the status from the rollout tail
    /// (`derive_status`). Returns [`CodexObservation`].
    fn poll_status(&self) -> CodexObservation;
    /// Sleep `ms` between polls (seamed so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic-ish clock in ms (the timeout deadline).
    fn now_ms(&self) -> i64;
}

/// The codex wait loop, PURE over [`CodexWaitDeps`]. Polls until the thread is
/// observed [`CodexObservation::Idle`] (→ [`WaitStatusOutcome::Done`]) or the
/// deadline elapses (→ [`WaitStatusOutcome::Timeout`]). There is NO "session
/// exited" outcome here: an unreachable daemon degrades to the rollout-tail
/// channel inside the deps rather than ending the wait (codex-p2-spec section 7.6
/// — the durable rollout is the truth even when the daemon is gone). A
/// `timeout_ms <= 0` polls once and (if not already idle) times out.
pub fn run_codex_wait_loop(
    deps: &dyn CodexWaitDeps,
    timeout_ms: i64,
    poll_ms: u64,
) -> WaitStatusOutcome {
    let start = deps.now_ms();
    loop {
        if let CodexObservation::Idle = deps.poll_status() {
            return WaitStatusOutcome::Done;
        }
        if deps.now_ms() - start >= timeout_ms {
            return WaitStatusOutcome::Timeout;
        }
        deps.sleep(poll_ms);
    }
}

/// The real (ws + rollout) [`CodexWaitDeps`] (codex-p2-spec section 7.6).
///
/// Channel discipline: while the live ws client is connected, each poll reads the
/// NEXT `thread/status/changed` broadcast (bounded by `notif_timeout`) and maps it
/// via [`crate::provider::codex::CodexProvider::parse_status`]; `Idle` ends the
/// wait, `Busy` keeps polling, an unmapped frame is `Pending`. W9 FIX Mo-1: on a
/// QUIET poll (`Ok(None)` — no status notification within the bound) the connected
/// channel ALSO cross-checks the durable rollout tail via `derive_status`; a
/// balanced (Idle) tail resolves Done. This closes the idle-in-the-connect-window
/// hole where a thread goes idle (the idle broadcast already past) and the socket
/// stays quiet-alive — the notification stream alone would Pending to the timeout
/// on an actually-idle thread. On a connection drop
/// ([`crate::provider::codex::RpcError::Closed`] or a transport error) the deps
/// LATCH to the rollout-tail channel: every subsequent poll reads the rollout tail
/// and `derive_status` (the same connectionless path W5's ls uses), so the wait
/// still resolves if the daemon is gone. A `None` rollout path (unresolvable
/// transcript) on either channel yields `Pending` (polls to the timeout — the
/// truthful "cannot tell" state).
pub struct RealCodexWaitDeps<'a> {
    /// The connected ws client (live channel). `None` if the initial connect
    /// failed — the deps start on the rollout-tail channel.
    pub rpc: Option<&'a dyn crate::provider::codex::AppServerRpc>,
    /// The rollout path for the fallback channel (`None` when unresolvable).
    pub rollout_path: Option<PathBuf>,
    /// Per-poll notification read bound on the live channel.
    pub notif_timeout: std::time::Duration,
    /// Latched to true once the live channel drops (then rollout-tail only).
    pub fell_back: Cell<bool>,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
}

impl<'a> RealCodexWaitDeps<'a> {
    pub fn new(
        rpc: Option<&'a dyn crate::provider::codex::AppServerRpc>,
        rollout_path: Option<PathBuf>,
        notif_timeout: std::time::Duration,
        clock: &'a dyn Clock,
        sleeper: &'a dyn Sleeper,
    ) -> Self {
        Self {
            // If we never connected, start on the fallback channel.
            fell_back: Cell::new(rpc.is_none()),
            rpc,
            rollout_path,
            notif_timeout,
            clock,
            sleeper,
        }
    }

    /// Derive the status from the rollout tail (the fallback channel). `None` path
    /// or an unreadable/anchorless tail → `Pending`.
    fn rollout_observe(&self) -> CodexObservation {
        use crate::model::SessionStatus;
        use crate::provider::codex::{derive_status, read_lines};
        let Some(path) = &self.rollout_path else {
            return CodexObservation::Pending;
        };
        match derive_status(&read_lines(path)) {
            Some(SessionStatus::Idle) => CodexObservation::Idle,
            Some(SessionStatus::Busy) => CodexObservation::Busy,
            _ => CodexObservation::Pending,
        }
    }
}

impl CodexWaitDeps for RealCodexWaitDeps<'_> {
    fn poll_status(&self) -> CodexObservation {
        use crate::model::SessionStatus;
        use crate::provider::codex::{CodexProvider, RpcError};
        use crate::provider::Provider;

        if self.fell_back.get() {
            return self.rollout_observe();
        }
        // Live channel: read the next notification (bounded). A drop latches the
        // fallback and is answered from the rollout tail THIS poll.
        let Some(rpc) = self.rpc else {
            self.fell_back.set(true);
            return self.rollout_observe();
        };
        match rpc.next_notification(self.notif_timeout) {
            Ok(Some(notif)) => {
                // Map the notification OBJECT via parse_status (the protocol
                // channel). Non-status frames map to None → Pending.
                let raw = serde_json::json!({
                    "method": notif.method,
                    "params": notif.params,
                });
                match CodexProvider.parse_status(&raw) {
                    Some(SessionStatus::Idle) => CodexObservation::Idle,
                    Some(SessionStatus::Busy) => CodexObservation::Busy,
                    _ => CodexObservation::Pending,
                }
            }
            // A quiet socket (no notification before the bound). W9 FIX Mo-1: a
            // quiet-but-ALIVE socket is NOT proof the thread is still busy — the
            // thread can go idle in the connect window (the idle broadcast already
            // past) and then sit silent, which the notification stream alone never
            // resolves (it would Pending to the timeout on an actually-idle thread).
            // So while connected we ALSO cross-check the DURABLE rollout tail: if the
            // tail shows Idle (balanced turns), resolve Done. The live channel thus
            // watches BOTH the notification stream AND the rollout tail, not just the
            // transition broadcast. A Busy/None tail → keep polling (the turn is
            // genuinely running, or we cannot tell yet).
            Ok(None) => match self.rollout_observe() {
                CodexObservation::Idle => CodexObservation::Idle,
                // Busy/Pending: stay on the live channel, keep polling (do NOT latch
                // the fallback — the socket is still alive; this is only a tail peek).
                _ => CodexObservation::Pending,
            },
            // The connection closed / dropped → latch the rollout-tail channel and
            // answer from it this poll (the daemon-unreachable fallback).
            Err(RpcError::Closed) | Err(RpcError::Transport(_)) | Err(RpcError::Timeout) => {
                self.fell_back.set(true);
                self.rollout_observe()
            }
            Err(_) => CodexObservation::Pending,
        }
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Scripted deps: status + content timelines keyed by poll count.
    struct FakeWait<S, C>
    where
        S: Fn(u32) -> Option<&'static str>,
        C: Fn(u32) -> Result<bool, String>,
    {
        polls: Cell<u32>,
        t: Cell<i64>,
        status_at: S,
        content_at: C,
        warns: RefCell<Vec<String>>,
    }
    impl<S, C> FakeWait<S, C>
    where
        S: Fn(u32) -> Option<&'static str>,
        C: Fn(u32) -> Result<bool, String>,
    {
        fn new(status_at: S, content_at: C) -> Self {
            Self {
                polls: Cell::new(0),
                t: Cell::new(0),
                status_at,
                content_at,
                warns: RefCell::new(Vec::new()),
            }
        }
    }
    impl<S, C> WaitContentDeps for FakeWait<S, C>
    where
        S: Fn(u32) -> Option<&'static str>,
        C: Fn(u32) -> Result<bool, String>,
    {
        fn read_status(&self) -> Option<String> {
            (self.status_at)(self.polls.get()).map(str::to_string)
        }
        fn poll_turn_end(&self) -> Result<bool, String> {
            (self.content_at)(self.polls.get())
        }
        fn warn(&self, msg: &str) {
            self.warns.borrow_mut().push(msg.to_string());
        }
        fn sleep(&self, ms: u64) {
            self.t.set(self.t.get() + ms as i64);
            self.polls.set(self.polls.get() + 1);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
    }

    /// Content probe that never reports evidence (the status-only baseline).
    fn no_content(_p: u32) -> Result<bool, String> {
        Ok(false)
    }

    // --- preserved status-keyed rows (pre-fix contract, adapted) -----------

    #[test]
    fn busy_then_idle_is_done() {
        let f = FakeWait::new(
            |p| if p == 0 { Some("busy") } else { Some("idle") },
            no_content,
        );
        assert_eq!(
            run_wait_content_loop(&f, 60_000, 500),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn shell_latches_sawbusy_too() {
        // shell counts as busy for the latch (status.ts:373).
        let f = FakeWait::new(
            |p| if p == 0 { Some("shell") } else { Some("idle") },
            no_content,
        );
        assert_eq!(
            run_wait_content_loop(&f, 60_000, 500),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn idle_without_evidence_keeps_polling_to_timeout() {
        // Idle-without-evidence rule (was "W5 invariant", a4-spec W5): idle from
        // the very first poll, NEVER busy, NO content evidence → polls to the
        // deadline → Timeout (NOT Done).
        let f = FakeWait::new(|_| Some("idle"), no_content);
        assert_eq!(
            run_wait_content_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    #[test]
    fn unreadable_status_is_session_exited() {
        let f = FakeWait::new(|_| None, no_content);
        assert_eq!(
            run_wait_content_loop(&f, 60_000, 500),
            WaitStatusOutcome::SessionExited
        );
    }

    #[test]
    fn busy_unreadable_mid_wait_is_session_exited() {
        let f = FakeWait::new(|p| if p == 0 { Some("busy") } else { None }, no_content);
        assert_eq!(
            run_wait_content_loop(&f, 60_000, 500),
            WaitStatusOutcome::SessionExited
        );
    }

    #[test]
    fn stays_busy_to_timeout() {
        let f = FakeWait::new(|_| Some("busy"), no_content);
        assert_eq!(
            run_wait_content_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    // --- W6 content rows (the wave's differential + controls) --------------

    #[test]
    fn w6_window_content_evidence_completes_idle_only_statuses() {
        // THE W6 DIFFERENTIAL ROW (first-driven; committed RED-pre-fix evidence
        // in test/golden/mutation/EVIDENCE-wart-wave-m3.txt): the turn started
        // AND ended inside one poll interval — status reads `idle` on EVERY poll
        // (saw_busy can NEVER latch, so status alone can NEVER complete this
        // row; immune to status-shadowing by construction). The turn's end_turn
        // record appears past the offset at poll 1 → Done. A dead/miswired
        // content latch (e.g. wrong stop_reason field path) times out → RED.
        let f = FakeWait::new(|_| Some("idle"), |p| Ok(p >= 1));
        assert_eq!(
            run_wait_content_loop(&f, 60_000, 500),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn negctl_content_alone_does_not_complete_while_busy() {
        // NEGATIVE CONTROL 1: evidence latched but status never reaches idle →
        // keep polling to Timeout. Content is EVIDENCE, not completion.
        let f = FakeWait::new(|_| Some("busy"), |_| Ok(true));
        assert_eq!(
            run_wait_content_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    #[test]
    fn negctl_no_stale_evidence_before_offset() {
        // NEGATIVE CONTROL 2 (stale-record guard): the probe contract is "past
        // the entry offset" — a probe that finds nothing new (old end_turn
        // records live BEFORE the offset) reports Ok(false) and idle-only
        // statuses poll to Timeout. (The offset semantics themselves are pinned
        // by the RealTurnEndProbe unit rows below.)
        let f = FakeWait::new(|_| Some("idle"), no_content);
        assert_eq!(
            run_wait_content_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    #[test]
    fn mutation_content_key_disabled_times_out_on_w6_window() {
        // MUTATION ROW (teeth): the W6-window timeline with the content key
        // disabled (probe never reports) MUST time out — proves the new latch,
        // not some other change, closes the window.
        let f = FakeWait::new(|_| Some("idle"), no_content);
        assert_eq!(
            run_wait_content_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    #[test]
    fn degrade_on_source_error_warns_once_and_stays_status_keyed() {
        // INTEGRITY ROW: source error at poll 0 → ONE warn, content key off;
        // busy→idle later still completes via the status latch.
        let f = FakeWait::new(
            |p| match p {
                0 | 1 => Some("busy"),
                _ => Some("idle"),
            },
            |_| Err("transcript shrank below the entry offset (10 < 20 bytes)".into()),
        );
        assert_eq!(
            run_wait_content_loop(&f, 60_000, 500),
            WaitStatusOutcome::Done
        );
        assert_eq!(f.warns.borrow().len(), 1, "exactly one degradation warn");
    }

    #[test]
    fn degrade_does_not_false_done_after_shrink() {
        // INTEGRITY ROW 2: idle-only statuses + a probe that errors (shrunk
        // source) must NOT produce Done — degrade means status-only, and status
        // alone has no evidence here → Timeout.
        let f = FakeWait::new(|_| Some("idle"), |_| Err("shrank".into()));
        assert_eq!(
            run_wait_content_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
        assert_eq!(f.warns.borrow().len(), 1);
    }

    // =======================================================================
    // WP-B5-ii-a guarantees (i) + (iii) — daemon-death FAIL-CLOSED, exercised at
    // the RealWaitContentDeps MAPPING (the status/poll state machine — NOT the
    // frozen content loop; the new guards/exits live here). Cheap unit-layer
    // mirrors of the integration test's (i)/(iii) legs, on the default floor.
    //
    // The fix-shaped mutations these red under live in this non-frozen mapping
    // (`RealWaitContentDeps::read_status` / `::poll_turn_end`), NOT in the frozen
    // `run_wait_content_loop`:
    //   (i)   poll_turn_end: `Degraded(why) => Err(why)`  → mutate to `=> Ok(true)`
    //         (a torn/missing result maps to turn-end) → FALSE Done.
    //   (iii) read_status:   `Live(Offline) => return None` → mutate to `=> {}`
    //         (trust the disk fallback) → the daemon-down exit is lost → hangs to
    //         Timeout instead of SessionExited.
    // =======================================================================

    /// A fixed channel control-status observation (daemon death = `Live(Offline)`
    /// once the daemon republishes Breaker/Eof-abort, or `Down` in channel-down
    /// mode).
    struct FixedStatusSource(ChannelStatusObservation);
    impl ChannelStatusSource for FixedStatusSource {
        fn poll_status(&self) -> ChannelStatusObservation {
            self.0
        }
    }
    /// A fixed disk status-fallback (the last daemon-written `pid.json` value —
    /// STALE after the daemon dies, since nobody flips it now).
    struct FixedFallback(Option<&'static str>);
    impl StatusFallback for FixedFallback {
        fn read_disk_status(&self) -> Option<String> {
            self.0.map(str::to_string)
        }
    }
    /// A fixed turn-completion probe verdict (the channel-down transcript barrier's
    /// Visible/Pending/Degraded).
    struct FixedCompletion(TurnCompletionProbe);
    impl TurnCompletion for FixedCompletion {
        fn poll_completion(&self) -> TurnCompletionProbe {
            self.0.clone()
        }
    }

    /// (iii) DAEMON-DOWN → SESSIONEXITED (one of the three independent non-hang
    /// exits). The daemon's death is republished as control `Offline`; the mapping
    /// turns it into `read_status == None`, which the loop terminates on as
    /// `SessionExited` — bounded, never a hang, never a false Done. The STALE disk
    /// `busy` (which a naive reader would trust forever) is deliberately present to
    /// prove the channel-Offline path takes precedence over it.
    ///
    /// RED-BEFORE (mutate `read_status`: `Live(Offline) => return None` → `=> {}`):
    /// the daemon-down exit is lost, the loop falls back to the stale disk `busy`
    /// and polls to the deadline → this assert reds with
    /// `left: Timeout, right: SessionExited`.
    #[test]
    fn b5ii_iii_daemon_offline_is_session_exited_not_stale_busy() {
        use crate::boot::RealSleeper;
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealWaitContentDeps {
            status_source: Some(Box::new(FixedStatusSource(ChannelStatusObservation::Live(
                ChannelStatus::Offline,
            )))),
            status_fallback: Box::new(FixedFallback(Some("busy"))), // stale, must be ignored
            completion: Some(Box::new(FixedCompletion(TurnCompletionProbe::Pending))),
            clock: &clock,
            sleeper: &sleeper,
        };
        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::SessionExited,
            "daemon Offline → SessionExited (never the stale disk busy, never a hang)"
        );
    }

    /// (iii) DAEMON-DEAD STALE-BUSY → TIMEOUT (the deadline exit). In channel-down
    /// mode (production today) a dead daemon leaves `pid.json` stuck at the last
    /// `busy` — nobody flips it idle — and the completion stays Pending (no result).
    /// The loop is BOUNDED by the deadline → Timeout. Proves `qd wait` does NOT
    /// hang on a dead daemon even when the status never resolves.
    #[test]
    fn b5ii_iii_dead_daemon_stale_busy_times_out_not_hangs() {
        use crate::boot::RealSleeper;
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealWaitContentDeps {
            status_source: None, // channel-down mode
            status_fallback: Box::new(FixedFallback(Some("busy"))),
            completion: Some(Box::new(FixedCompletion(TurnCompletionProbe::Pending))),
            clock: &clock,
            sleeper: &sleeper,
        };
        assert_eq!(
            run_wait_content_loop(&deps, 3_000, 0),
            WaitStatusOutcome::Timeout,
            "dead daemon, status stuck busy → bounded Timeout, never a hang"
        );
    }

    /// (i) CONTROL FAILS CLOSED — a Degraded completion (the channel-down
    /// transcript barrier on a torn/missing `result`) maps to `Err` → status-only,
    /// and with the status `idle` but NO busy/turn-end evidence the loop polls to
    /// Timeout. It NEVER latches Done from a torn result — no false-done.
    ///
    /// RED-BEFORE (mutate `poll_turn_end`: `Degraded(why) => Err(why)` →
    /// `Degraded(_) => Ok(true)`): the torn result latches `saw_turn_end`, idle +
    /// evidence → Done, and this assert reds with `left: Done, right: Timeout` —
    /// the exact false-done the guarantee forbids.
    #[test]
    fn b5ii_i_degraded_result_never_false_dones() {
        use crate::boot::RealSleeper;
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealWaitContentDeps {
            status_source: None, // channel-down mode → completion is the transcript barrier
            status_fallback: Box::new(FixedFallback(Some("idle"))),
            completion: Some(Box::new(FixedCompletion(TurnCompletionProbe::Degraded(
                "transcript shrank below the entry offset (10 < 20 bytes)".into(),
            )))),
            clock: &clock,
            sleeper: &sleeper,
        };
        assert_eq!(
            run_wait_content_loop(&deps, 3_000, 0),
            WaitStatusOutcome::Timeout,
            "Degraded result → status-only, idle-without-evidence → Timeout (NEVER false Done)"
        );
    }

    /// (i) POSITIVE CONTROL — the SAME wiring with a genuine `Visible` result (a
    /// real, untorn `result`) DOES complete Done. Proves the no-false-done guard
    /// above is not vacuously timing out: a real result still latches.
    #[test]
    fn b5ii_i_real_result_completes_done() {
        use crate::boot::RealSleeper;
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealWaitContentDeps {
            status_source: None,
            status_fallback: Box::new(FixedFallback(Some("idle"))),
            completion: Some(Box::new(FixedCompletion(TurnCompletionProbe::Visible))),
            clock: &clock,
            sleeper: &sleeper,
        };
        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "a real Visible result still completes Done (guard is not vacuous)"
        );
    }

    // --- slice_has_turn_end field-path rows (red-team R4) ------------------

    #[test]
    fn turn_end_matches_nested_message_stop_reason() {
        let line = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#;
        assert!(slice_has_turn_end(line));
    }

    #[test]
    fn turn_end_ignores_top_level_stop_reason() {
        // R4: a TOP-LEVEL stop_reason is NOT the transcript shape — must not latch.
        let line = r#"{"type":"assistant","stop_reason":"end_turn"}"#;
        assert!(!slice_has_turn_end(line));
    }

    #[test]
    fn turn_end_ignores_non_assistant_and_non_end_turn() {
        let user = r#"{"type":"user","message":{"stop_reason":"end_turn"}}"#;
        let tooluse = r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#;
        let null_stop = r#"{"type":"assistant","message":{"stop_reason":null}}"#;
        assert!(!slice_has_turn_end(user));
        assert!(!slice_has_turn_end(tooluse));
        assert!(!slice_has_turn_end(null_stop));
        // Mixed slice with one real end_turn among noise → latches.
        let mixed = format!(
            "{user}\n{tooluse}\nnot-json\n{}",
            r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#
        );
        assert!(slice_has_turn_end(&mixed));
    }

    // --- RealTurnEndProbe offset semantics (fs-backed) ----------------------

    #[test]
    fn probe_offset_excludes_pre_entry_records() {
        // Stale-record guard at the REAL probe: an end_turn BEFORE the entry
        // offset does not count; one appended PAST it does.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let old = "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n";
        std::fs::write(&p, old).unwrap();
        let entry = std::fs::metadata(&p).unwrap().len();

        let probe = RealTurnEndProbe::new(p.clone(), entry);
        assert_eq!(probe.poll(), Ok(false), "pre-entry record must not latch");

        std::fs::write(
            &p,
            format!(
                "{old}{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"end_turn\"}}}}\n"
            ),
        )
        .unwrap();
        assert_eq!(probe.poll(), Ok(true), "post-entry record latches");
    }

    #[test]
    fn probe_shrunk_file_is_integrity_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "0123456789012345678901234567890123456789\n").unwrap();
        let entry = std::fs::metadata(&p).unwrap().len();
        let probe = RealTurnEndProbe::new(p.clone(), entry);
        std::fs::write(&p, "short\n").unwrap();
        assert!(probe.poll().is_err(), "shrunk-past-offset must degrade");
    }

    #[test]
    fn probe_absent_file_keeps_polling() {
        // Absent transcript is NOT integrity loss for the additive probe.
        let probe = RealTurnEndProbe::new(PathBuf::from("/nonexistent/qd-wait/t.jsonl"), 0);
        assert_eq!(probe.poll(), Ok(false));
    }

    // --- §6.4 TurnCompletion interceptor (the swap-point (B) re-implements) --

    #[test]
    fn turn_completion_interceptor_maps_probe_states() {
        // The §6.4 seam contract: RealTurnEndProbe's Ok(false)/Ok(true)/Err map
        // onto Pending/Visible/Degraded — exactly the three states (B)'s
        // stream-json `result` impl will produce. A swapped impl that honors this
        // enum drops into the loop without any call-site change.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Pending: a slice past the offset with no end_turn yet.
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
        let probe = RealTurnEndProbe::new(p.clone(), 0);
        assert_eq!(probe.poll_completion(), TurnCompletionProbe::Pending);

        // Visible: append an end_turn record past the anchor → latch.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        let end_turn = r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#;
        writeln!(f, "{end_turn}").unwrap();
        assert_eq!(probe.poll_completion(), TurnCompletionProbe::Visible);

        // Degraded: anchored past a now-shrunk file → degrade, never re-scan-from-0.
        let q = dir.path().join("s.jsonl");
        std::fs::write(&q, "0123456789012345678901234567890123456789\n").unwrap();
        let entry = std::fs::metadata(&q).unwrap().len();
        let probe2 = RealTurnEndProbe::new(q.clone(), entry);
        std::fs::write(&q, "x\n").unwrap();
        assert!(matches!(
            probe2.poll_completion(),
            TurnCompletionProbe::Degraded(_)
        ));
    }

    /// A test [`Sleeper`] that appends `line` to `path` on its FIRST sleep — models
    /// the turn completing INSIDE one poll interval (the W6 window): the end_turn
    /// record becomes VISIBLE in the transcript between two idle polls.
    struct AppendOnFirstSleep {
        path: PathBuf,
        line: String,
        fired: Cell<bool>,
    }
    impl Sleeper for AppendOnFirstSleep {
        fn sleep_ms(&self, _ms: u64) {
            if !self.fired.replace(true) {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&self.path)
                    .unwrap();
                f.write_all(self.line.as_bytes()).unwrap();
                f.flush().unwrap();
            }
        }
    }

    #[test]
    fn real_barrier_closes_w6_window_end_to_end() {
        // CONSUMER-EXERCISING red/green through the REAL fs-backed visibility
        // barrier (not the FakeWait scripts): an idle-only session — `saw_busy` can
        // NEVER latch, so status alone can NEVER complete it (the W6 window) —
        // whose end_turn record becomes VISIBLE in the transcript between two polls
        // latches Done via the `TurnCompletion` barrier wired into the REAL deps.
        // RED-WITHOUT is its sibling `real_barrier_removed_w6_window_times_out`.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid.json");
        std::fs::write(&pid_file, r#"{"status":"idle"}"#).unwrap();
        let jsonl = dir.path().join("t.jsonl");
        // At wait entry the turn is mid-flight: a transcript with NO end_turn yet.
        std::fs::write(&jsonl, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
        let entry = std::fs::metadata(&jsonl).unwrap().len();
        let probe = RealTurnEndProbe::new(jsonl.clone(), entry);

        let clock = StepClock::new(1000);
        let sleeper = AppendOnFirstSleep {
            path: jsonl.clone(),
            line: "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n"
                .to_string(),
            fired: Cell::new(false),
        };
        // WP-B3: channel-DOWN mode (status_source None) preserves today's behavior:
        // pid.json status (StatusFallback) + the (A) transcript barrier completion.
        let deps = RealWaitContentDeps {
            status_source: None,
            status_fallback: Box::new(PidFileStatus { pid_file }),
            completion: Some(Box::new(probe)),
            clock: &clock,
            sleeper: &sleeper,
        };
        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 500),
            WaitStatusOutcome::Done,
            "the end_turn appended between idle polls becomes visible → Done"
        );
    }

    #[test]
    fn real_barrier_removed_w6_window_times_out() {
        // FIX-SHAPED MUTATION (teeth): the SAME idle-only W6 timeline with the
        // barrier REMOVED (`completion: None` = the pre-fix status-only keying)
        // polls idle to TIMEOUT on a completed turn — the live false "priming
        // failed"/timeout bug this WP closes. Removing the barrier reds the pair.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid.json");
        std::fs::write(&pid_file, r#"{"status":"idle"}"#).unwrap();
        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper; // poll_ms=0; the turn-end is never observed
        let deps = RealWaitContentDeps {
            status_source: None,
            status_fallback: Box::new(PidFileStatus { pid_file }),
            completion: None,
            clock: &clock,
            sleeper: &sleeper,
        };
        assert_eq!(
            run_wait_content_loop(&deps, 1500, 0),
            WaitStatusOutcome::Timeout,
            "no visibility barrier ⇒ idle-only polls to timeout (the pre-fix bug)"
        );
    }

    #[test]
    fn probe_under_concurrent_appends_never_false_completes_then_latches() {
        // CONCURRENCY/LOAD (DoD §6.5, k≥2): two writer threads hammer the transcript
        // with NON-end_turn appends (O_APPEND — claude's write mode) while the probe
        // polls concurrently. The barrier must NEVER false-latch on the churn, never
        // panic/torn-parse, never spuriously degrade — and MUST latch once a real
        // end_turn lands past the anchor.
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
        let entry = std::fs::metadata(&p).unwrap().len();
        let probe = RealTurnEndProbe::new(p.clone(), entry);

        // k=2 writers, each appending a BOUNDED run of throttled noise lines (so the
        // file stays small — the realistic per-turn append rate, not a fork bomb).
        let done = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for w in 0..2u32 {
            let pp = p.clone();
            writers.push(std::thread::spawn(move || {
                // tool_use, NOT end_turn — must never latch the barrier.
                let noise = format!(
                    "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"tool_use\",\"w\":{w}}}}}\n"
                );
                for _ in 0..200 {
                    let mut f = std::fs::OpenOptions::new().append(true).open(&pp).unwrap();
                    let _ = f.write_all(noise.as_bytes());
                    std::thread::sleep(Duration::from_micros(200));
                }
            }));
        }

        // Poll concurrently WHILE the writers churn: must stay Pending (no end_turn),
        // never error/panic/torn-parse.
        let done2 = done.clone();
        let pp = p.clone();
        let poller = std::thread::spawn(move || {
            let probe = RealTurnEndProbe::new(pp, entry);
            while !done2.load(Ordering::Relaxed) {
                match probe.poll_completion() {
                    TurnCompletionProbe::Pending => {}
                    TurnCompletionProbe::Visible => panic!("false-completed on non-end_turn churn"),
                    TurnCompletionProbe::Degraded(why) => {
                        panic!("spurious degrade under append churn: {why}")
                    }
                }
            }
        });

        for h in writers {
            h.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        poller.join().unwrap();

        // Land a real end_turn past the anchor; confirm the barrier latches.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n")
            .unwrap();
        f.flush().unwrap();
        assert_eq!(probe.poll_completion(), TurnCompletionProbe::Visible);
    }

    #[test]
    fn turn_end_probe_latency_p50_p99() {
        // LATENCY BUDGET (DoD §6.5) on the FIXED path: the per-poll read+parse+scan
        // over a realistic transcript (~200 records, no end_turn past offset — the
        // worst case where the whole slice is scanned every time). Budget: p99 <
        // 50ms (the real poll interval is 500ms; the probe is a small fraction of one
        // poll). A fresh probe per sample defeats the high-water skip → full work each.
        use std::time::Instant;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"tool_use\",\"i\":{i}}}}}\n"
            ));
        }
        std::fs::write(&p, &body).unwrap();

        let mut samples = Vec::with_capacity(300);
        for _ in 0..300 {
            let probe = RealTurnEndProbe::new(p.clone(), 0);
            let t = Instant::now();
            let _ = probe.poll_completion();
            samples.push(t.elapsed());
        }
        samples.sort();
        let p50 = samples[samples.len() / 2];
        let p99 = samples[samples.len() * 99 / 100];
        println!(
            "TurnCompletion probe read+parse+scan latency: p50={p50:?} p99={p99:?} (200 records)"
        );
        assert!(
            p99.as_millis() < 50,
            "probe p99 {p99:?} exceeds 50ms budget"
        );
    }

    // =======================================================================
    // WP-B3 (§6.0-purity keystone, R1 / §H.2 / gate D2): turn-completion +
    // control-status routed through the daemon CHANNEL, not a disk read.
    // =======================================================================

    /// A scripted [`ChannelStatusSource`]: returns the observation for its own
    /// invocation index (call 0 = the first `read_status`, etc.).
    struct ScriptStatus<F: Fn(u32) -> ChannelStatusObservation> {
        calls: Cell<u32>,
        at: F,
    }
    impl<F: Fn(u32) -> ChannelStatusObservation> ScriptStatus<F> {
        fn new(at: F) -> Self {
            Self {
                calls: Cell::new(0),
                at,
            }
        }
    }
    impl<F: Fn(u32) -> ChannelStatusObservation> ChannelStatusSource for ScriptStatus<F> {
        fn poll_status(&self) -> ChannelStatusObservation {
            let n = self.calls.get();
            self.calls.set(n + 1);
            (self.at)(n)
        }
    }

    /// A scripted [`ChannelTurnSource`]: the `result`-event timeline keyed by its
    /// own invocation index.
    struct ScriptTurn<F: Fn(u32) -> ChannelTurnObservation> {
        calls: Cell<u32>,
        at: F,
    }
    impl<F: Fn(u32) -> ChannelTurnObservation> ScriptTurn<F> {
        fn new(at: F) -> Self {
            Self {
                calls: Cell::new(0),
                at,
            }
        }
    }
    impl<F: Fn(u32) -> ChannelTurnObservation> ChannelTurnSource for ScriptTurn<F> {
        fn poll_result(&self) -> ChannelTurnObservation {
            let n = self.calls.get();
            self.calls.set(n + 1);
            (self.at)(n)
        }
    }

    /// A [`StatusFallback`] that RECORDS every disk-status read (the §H.2 canary on
    /// the `pid.json` read) and returns a scripted status if/when called.
    struct InstrumentedDisk {
        calls: std::rc::Rc<Cell<u32>>,
        returns: Option<&'static str>,
    }
    impl StatusFallback for InstrumentedDisk {
        fn read_disk_status(&self) -> Option<String> {
            self.calls.set(self.calls.get() + 1);
            self.returns.map(str::to_string)
        }
    }

    /// A [`TurnCompletion`] that RECORDS every call (the §H.2 canary on the
    /// transcript-barrier read) and returns a scripted probe if/when called. Stands
    /// in for the (A) [`RealTurnEndProbe`] transcript read as the channel-down
    /// fallback — gate R1 demands ZERO transcript read on the healthy path.
    struct InstrumentedProbe {
        calls: std::rc::Rc<Cell<u32>>,
        returns: TurnCompletionProbe,
    }
    impl TurnCompletion for InstrumentedProbe {
        fn poll_completion(&self) -> TurnCompletionProbe {
            self.calls.set(self.calls.get() + 1);
            self.returns.clone()
        }
    }

    /// `RealSleeper` (instant, poll_ms=0) + `StepClock` build the loop's time; the
    /// scripted channel sources own the per-poll timeline. Helper to keep the deps
    /// construction terse across the rows below.
    fn channel_deps<'a>(
        status_source: Option<Box<dyn ChannelStatusSource>>,
        status_fallback: Box<dyn StatusFallback>,
        completion: Option<Box<dyn TurnCompletion>>,
        clock: &'a StepClock,
        sleeper: &'a crate::boot::RealSleeper,
    ) -> RealWaitContentDeps<'a> {
        RealWaitContentDeps {
            status_source,
            status_fallback,
            completion,
            clock,
            sleeper,
        }
    }

    // --- consumer-exercising red/green (the load-bearing differential) ------

    #[test]
    fn channel_w6_window_result_completes_idle_only() {
        // GREEN (load-bearing): the W6 window — a turn whose status reads `idle` on
        // EVERY poll (saw_busy can NEVER latch) — completes because the daemon
        // republished the expected turn's `result` event over the CHANNEL. The
        // transcript barrier is never consulted (channel up).
        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Live(ChannelStatus::Idle));
        let turn = ScriptTurn::new(|p| {
            if p >= 1 {
                ChannelTurnObservation::Seen
            } else {
                ChannelTurnObservation::NotYet
            }
        });
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            // A fallback that would FALSE-complete if ever consulted on the healthy
            // path — proving completion comes from the channel, not this.
            fallback: Box::new(InstrumentedProbe {
                calls: std::rc::Rc::new(Cell::new(0)),
                returns: TurnCompletionProbe::Visible,
            }),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(PidFileStatus {
                pid_file: PathBuf::from("/nonexistent/b3/pid.json"),
            }),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "channel `result` latches Done on an idle-only W6 timeline"
        );
    }

    #[test]
    fn channel_result_dropped_times_out() {
        // FIX-SHAPED MUTATION (teeth): the SAME idle-only W6 timeline with the
        // channel `result` source DROPPED (always NotYet) AND the transcript
        // fallback never firing (channel up, so fallback not reached) → no evidence
        // → Timeout. Proves the channel `result` is load-bearing for this row.
        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Live(ChannelStatus::Idle));
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::NotYet);
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(InstrumentedProbe {
                calls: std::rc::Rc::new(Cell::new(0)),
                returns: TurnCompletionProbe::Pending,
            }),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(PidFileStatus {
                pid_file: PathBuf::from("/nonexistent/b3/pid.json"),
            }),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&deps, 1500, 0),
            WaitStatusOutcome::Timeout,
            "no channel `result` (and channel up ⇒ no fallback) ⇒ idle-only times out"
        );
    }

    #[test]
    fn channel_quiet_poll_no_false_done() {
        // FALSE-POSITIVE guard: a healthy channel reporting Busy + the `result` not
        // yet republished must NOT complete — no false Done on a quiet poll.
        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Live(ChannelStatus::Busy));
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::NotYet);
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(InstrumentedProbe {
                calls: std::rc::Rc::new(Cell::new(0)),
                returns: TurnCompletionProbe::Pending,
            }),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(PidFileStatus {
                pid_file: PathBuf::from("/nonexistent/b3/pid.json"),
            }),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&deps, 1500, 0),
            WaitStatusOutcome::Timeout,
            "busy + no result ⇒ keep polling, never false-Done"
        );
    }

    // --- §H.2 / gate D2 — THE §6.0-purity red/green test -------------------

    #[test]
    fn purity_healthy_channel_reads_no_disk_no_transcript() {
        // §H.2 GREEN (THE keystone deliverable): on the HEALTHY channel path the
        // turn completes via the channel `result` + channel status — and NEITHER the
        // `pid.json` disk-status read NOR the transcript-barrier read is EVER
        // performed (gate R1: zero disk-status AND zero transcript read on the
        // control path). Both fallbacks are INSTRUMENTED canaries; both must show 0.
        let disk_calls = std::rc::Rc::new(Cell::new(0));
        let probe_calls = std::rc::Rc::new(Cell::new(0));

        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Live(ChannelStatus::Idle));
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::Seen);
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(InstrumentedProbe {
                calls: probe_calls.clone(),
                returns: TurnCompletionProbe::Visible,
            }),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(InstrumentedDisk {
                calls: disk_calls.clone(),
                returns: Some("idle"),
            }),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );

        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "healthy channel completes the turn"
        );
        assert_eq!(
            disk_calls.get(),
            0,
            "§6.0 violation: a pid.json status read on the HEALTHY control path"
        );
        assert_eq!(
            probe_calls.get(),
            0,
            "gate R1 violation: a transcript read on the HEALTHY control path"
        );
    }

    #[test]
    fn purity_channel_down_reads_both_fallbacks() {
        // §H.2 TEETH (the canary works): forced channel-DOWN — both seams report
        // Down — so the loop FALLS BACK to the `pid.json` status read AND the
        // transcript barrier, and completes from them. Both canaries MUST fire
        // (proving the GREEN test's zeros mean "never called", not "uninstrumented").
        let disk_calls = std::rc::Rc::new(Cell::new(0));
        let probe_calls = std::rc::Rc::new(Cell::new(0));

        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Down);
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::Down);
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(InstrumentedProbe {
                calls: probe_calls.clone(),
                returns: TurnCompletionProbe::Visible,
            }),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(InstrumentedDisk {
                calls: disk_calls.clone(),
                returns: Some("idle"),
            }),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );

        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "channel-down resolves via the disk + transcript fallbacks"
        );
        assert!(
            disk_calls.get() >= 1,
            "channel-down MUST read pid.json (fallback)"
        );
        assert!(
            probe_calls.get() >= 1,
            "channel-down MUST read the transcript (fallback)"
        );
    }

    #[test]
    fn purity_pre_b3_shape_reads_disk_on_healthy_timeline() {
        // RED-BEFORE captured as a passing assertion: the PRE-B3 wiring
        // (`status_source: None` — no channel routing) reads `pid.json` even on the
        // SAME healthy-channel completion timeline. This is exactly what the §H.2
        // GREEN test forbids; pinning it here proves the fix (channel routing), not
        // some unrelated change, is what drives the disk read to zero.
        let disk_calls = std::rc::Rc::new(Cell::new(0));

        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        // The channel `result` would complete the turn — but with status_source None
        // the loop's STATUS still comes from disk every poll (the pre-fix sin).
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::Seen);
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(InstrumentedProbe {
                calls: std::rc::Rc::new(Cell::new(0)),
                returns: TurnCompletionProbe::Pending,
            }),
        };
        let deps = channel_deps(
            None, // PRE-B3: no channel status routing
            Box::new(InstrumentedDisk {
                calls: disk_calls.clone(),
                returns: Some("idle"),
            }),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done
        );
        assert!(
            disk_calls.get() >= 1,
            "pre-B3 shape reads pid.json on the control path (the violation B3 removes)"
        );
    }

    // --- §H.2 ENTRY-IDLE purity (RESIDUAL #1) — the PRE-LOOP gate -----------
    //     B3's loop purity test does NOT cover the entry-idle gate (it runs
    //     BEFORE the loop), so re-using it would be vacuous. These rows pin
    //     `entry_is_idle`: healthy channel ⇒ ZERO disk read; channel-down ⇒ the
    //     disk fallback answers (and is the ONLY thing that does).

    #[test]
    fn entry_idle_healthy_channel_reads_no_disk() {
        // §H.2 GREEN (entry-idle keystone): on the HEALTHY channel the entry-idle
        // gate is decided by the channel status — the disk-resolved status is NEVER
        // consulted. The `disk_idle` closure is an INSTRUMENTED canary; it must show
        // 0 calls. Live(Idle) ⇒ already-idle (short-circuit).
        let disk_calls = std::rc::Rc::new(Cell::new(0));
        let dc = disk_calls.clone();
        let idle = entry_is_idle(ChannelStatusObservation::Live(ChannelStatus::Idle), || {
            dc.set(dc.get() + 1);
            true
        });
        assert!(idle, "Live(Idle) ⇒ entry-idle");
        assert_eq!(
            disk_calls.get(),
            0,
            "§6.0 violation: a disk-status read on the HEALTHY entry-idle path"
        );

        // Live(Busy) ⇒ NOT entry-idle (proceed to the loop), still ZERO disk.
        let disk_calls = std::rc::Rc::new(Cell::new(0));
        let dc = disk_calls.clone();
        let busy = entry_is_idle(ChannelStatusObservation::Live(ChannelStatus::Busy), || {
            dc.set(dc.get() + 1);
            true // disk WOULD say idle — proving the channel, not disk, decides
        });
        assert!(!busy, "Live(Busy) ⇒ proceed (not entry-idle)");
        assert_eq!(disk_calls.get(), 0, "Busy channel still reads no disk");

        // Live(Offline) ⇒ NOT entry-idle (proceed; the loop maps Offline→exited).
        let off = entry_is_idle(
            ChannelStatusObservation::Live(ChannelStatus::Offline),
            || panic!("disk must not be read on a Live channel"),
        );
        assert!(!off, "Live(Offline) ⇒ proceed (not entry-idle)");
    }

    #[test]
    fn entry_idle_channel_down_reads_disk_fallback() {
        // §H.2 TEETH (the canary works): forced channel-DOWN ⇒ the entry-idle gate
        // FALLS BACK to the disk-resolved status — and that fallback IS what decides
        // (proving the GREEN test's zero means "never called", not "uninstrumented").
        let disk_calls = std::rc::Rc::new(Cell::new(0));
        let dc = disk_calls.clone();
        let idle = entry_is_idle(ChannelStatusObservation::Down, || {
            dc.set(dc.get() + 1);
            true // disk says idle
        });
        assert!(idle, "channel-down ⇒ disk fallback decides (idle here)");
        assert_eq!(
            disk_calls.get(),
            1,
            "channel-down MUST read the disk fallback"
        );

        // Channel-down + disk says NOT idle ⇒ proceed (the fallback governs both ways).
        let disk_calls = std::rc::Rc::new(Cell::new(0));
        let dc = disk_calls.clone();
        let busy = entry_is_idle(ChannelStatusObservation::Down, || {
            dc.set(dc.get() + 1);
            false
        });
        assert!(!busy, "channel-down + disk busy ⇒ proceed");
        assert_eq!(disk_calls.get(), 1);
    }

    // --- channel-down degradation (false-neg: never false-Done, never hang) -

    #[test]
    fn channel_down_fallback_resolves_like_today() {
        // FALSE-NEGATIVE guard: channel Down degrades to the (A) path and STILL
        // resolves exactly as today — disk status busy→idle + a transcript barrier
        // that becomes Visible → Done. Degrade is not a hang.
        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Down);
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::Down);
        // Disk reports busy then idle; transcript barrier Visible once → Done.
        let disk = ScriptedDisk::new(|p| if p == 0 { Some("busy") } else { Some("idle") });
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(ScriptedProbe::new(|p| {
                if p >= 1 {
                    TurnCompletionProbe::Visible
                } else {
                    TurnCompletionProbe::Pending
                }
            })),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(disk),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "channel-down still resolves via the (A) fallbacks"
        );
    }

    #[test]
    fn channel_down_idle_only_no_evidence_times_out() {
        // Channel Down + disk idle-only + transcript Pending → no evidence → Timeout
        // (degrade-not-false-Done; idle-without-evidence rule preserved).
        let clock = StepClock::new(1000);
        let sleeper = crate::boot::RealSleeper;
        let status = ScriptStatus::new(|_| ChannelStatusObservation::Down);
        let turn = ScriptTurn::new(|_| ChannelTurnObservation::Down);
        let disk = ScriptedDisk::new(|_| Some("idle"));
        let completion = ChannelTurnCompletion {
            source: Box::new(turn),
            fallback: Box::new(ScriptedProbe::new(|_| TurnCompletionProbe::Pending)),
        };
        let deps = channel_deps(
            Some(Box::new(status)),
            Box::new(disk),
            Some(Box::new(completion)),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&deps, 1500, 0),
            WaitStatusOutcome::Timeout,
            "channel-down idle-only with no evidence keeps polling (no false-Done)"
        );
    }

    // --- R-ii: the 3 WaitStatusOutcomes UNCHANGED vs today on equivalent ----
    //     channel inputs (frozen loop string/outcome contract preserved). ----

    /// A scripted [`StatusFallback`] (channel-down disk timeline), keyed by call
    /// index — for the degradation rows above and the parity table below.
    struct ScriptedDisk<F: Fn(u32) -> Option<&'static str>> {
        calls: Cell<u32>,
        at: F,
    }
    impl<F: Fn(u32) -> Option<&'static str>> ScriptedDisk<F> {
        fn new(at: F) -> Self {
            Self {
                calls: Cell::new(0),
                at,
            }
        }
    }
    impl<F: Fn(u32) -> Option<&'static str>> StatusFallback for ScriptedDisk<F> {
        fn read_disk_status(&self) -> Option<String> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            (self.at)(n).map(str::to_string)
        }
    }

    /// A scripted [`TurnCompletion`] (channel-down transcript timeline), keyed by
    /// call index.
    struct ScriptedProbe<F: Fn(u32) -> TurnCompletionProbe> {
        calls: Cell<u32>,
        at: F,
    }
    impl<F: Fn(u32) -> TurnCompletionProbe> ScriptedProbe<F> {
        fn new(at: F) -> Self {
            Self {
                calls: Cell::new(0),
                at,
            }
        }
    }
    impl<F: Fn(u32) -> TurnCompletionProbe> TurnCompletion for ScriptedProbe<F> {
        fn poll_completion(&self) -> TurnCompletionProbe {
            let n = self.calls.get();
            self.calls.set(n + 1);
            (self.at)(n)
        }
    }

    /// Build healthy-channel deps for the parity table: a status timeline + a turn
    /// timeline, no fallback ever expected to fire (channel up).
    fn parity_deps<'a>(
        status_at: fn(u32) -> ChannelStatusObservation,
        turn_at: fn(u32) -> ChannelTurnObservation,
        clock: &'a StepClock,
        sleeper: &'a crate::boot::RealSleeper,
    ) -> RealWaitContentDeps<'a> {
        let completion = ChannelTurnCompletion {
            source: Box::new(ScriptTurn::new(turn_at)),
            fallback: Box::new(ScriptedProbe::new(|_| TurnCompletionProbe::Pending)),
        };
        RealWaitContentDeps {
            status_source: Some(Box::new(ScriptStatus::new(status_at))),
            status_fallback: Box::new(PidFileStatus {
                pid_file: PathBuf::from("/nonexistent/b3/pid.json"),
            }),
            completion: Some(Box::new(completion)),
            clock,
            sleeper,
        }
    }

    #[test]
    fn channel_outcomes_match_today_on_equivalent_inputs() {
        let sleeper = crate::boot::RealSleeper;

        // (1) busy→idle WITH the result == today's busy_then_idle_is_done → Done.
        let c1 = StepClock::new(1000);
        let d1 = parity_deps(
            |p| {
                ChannelStatusObservation::Live(if p == 0 {
                    ChannelStatus::Busy
                } else {
                    ChannelStatus::Idle
                })
            },
            |p| {
                if p >= 1 {
                    ChannelTurnObservation::Seen
                } else {
                    ChannelTurnObservation::NotYet
                }
            },
            &c1,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&d1, 60_000, 0),
            WaitStatusOutcome::Done
        );

        // (2) channel Offline == today's status-unreadable → SessionExited (R-ii:
        // Live(Offline) maps to None → SessionExited, fail-closed, never a hang).
        let c2 = StepClock::new(1000);
        let d2 = parity_deps(
            |_| ChannelStatusObservation::Live(ChannelStatus::Offline),
            |_| ChannelTurnObservation::NotYet,
            &c2,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&d2, 60_000, 0),
            WaitStatusOutcome::SessionExited
        );

        // (3) busy forever == today's stays_busy_to_timeout → Timeout.
        let c3 = StepClock::new(1000);
        let d3 = parity_deps(
            |_| ChannelStatusObservation::Live(ChannelStatus::Busy),
            |_| ChannelTurnObservation::NotYet,
            &c3,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&d3, 1500, 0),
            WaitStatusOutcome::Timeout
        );

        // (4) idle-without-evidence == today's rule → Timeout (idle, no result).
        let c4 = StepClock::new(1000);
        let d4 = parity_deps(
            |_| ChannelStatusObservation::Live(ChannelStatus::Idle),
            |_| ChannelTurnObservation::NotYet,
            &c4,
            &sleeper,
        );
        assert_eq!(
            run_wait_content_loop(&d4, 1500, 0),
            WaitStatusOutcome::Timeout
        );
    }

    // --- codex P2 W6 wait loop (codex-p2-spec section 7.6) -----------------

    /// Scripted codex wait deps: an observation timeline keyed by poll count.
    struct FakeCodexWait<O>
    where
        O: Fn(u32) -> CodexObservation,
    {
        polls: Cell<u32>,
        t: Cell<i64>,
        obs_at: O,
    }
    impl<O> FakeCodexWait<O>
    where
        O: Fn(u32) -> CodexObservation,
    {
        fn new(obs_at: O) -> Self {
            Self {
                polls: Cell::new(0),
                t: Cell::new(0),
                obs_at,
            }
        }
    }
    impl<O> CodexWaitDeps for FakeCodexWait<O>
    where
        O: Fn(u32) -> CodexObservation,
    {
        fn poll_status(&self) -> CodexObservation {
            (self.obs_at)(self.polls.get())
        }
        fn sleep(&self, ms: u64) {
            self.t.set(self.t.get() + ms as i64);
            self.polls.set(self.polls.get() + 1);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
    }

    #[test]
    fn codex_wait_active_then_idle_resolves() {
        // The scripted notification sequence (active… then idle) resolves to Done:
        // Busy for the first few polls, then Idle (the thread/status/changed idle
        // transition the live channel observes).
        let f = FakeCodexWait::new(|p| {
            if p < 3 {
                CodexObservation::Busy
            } else {
                CodexObservation::Idle
            }
        });
        assert_eq!(
            run_codex_wait_loop(&f, 60_000, 500),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn codex_wait_idle_first_poll_is_done() {
        // Already idle on the first observation → Done immediately.
        let f = FakeCodexWait::new(|_| CodexObservation::Idle);
        assert_eq!(
            run_codex_wait_loop(&f, 60_000, 500),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn codex_wait_pending_polls_to_timeout() {
        // A quiet channel that never determines idle (all Pending) polls to the
        // deadline → Timeout. (The fallback's "cannot tell" state.)
        let f = FakeCodexWait::new(|_| CodexObservation::Pending);
        assert_eq!(
            run_codex_wait_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    #[test]
    fn codex_wait_busy_forever_times_out() {
        let f = FakeCodexWait::new(|_| CodexObservation::Busy);
        assert_eq!(
            run_codex_wait_loop(&f, 1500, 500),
            WaitStatusOutcome::Timeout
        );
    }

    // --- the REAL deps' daemon-unreachable → rollout-tail fallback ----------

    use crate::provider::codex::{ClientInfo, InitializeResult};
    use crate::provider::codex::{Notification, RpcError, SteerOutcome};

    /// A monotonic test clock that advances a fixed step each read (so the loop's
    /// deadline arithmetic is exercised without real time).
    struct StepClock {
        t: Cell<i64>,
        step: i64,
    }
    impl StepClock {
        fn new(step: i64) -> Self {
            Self {
                t: Cell::new(0),
                step,
            }
        }
    }
    impl Clock for StepClock {
        fn now_ms(&self) -> i64 {
            let v = self.t.get();
            self.t.set(v + self.step);
            v
        }
    }

    /// A fixture rpc whose `next_notification` returns a configurable result; used
    /// to drive the live channel + the drop→fallback latch in RealCodexWaitDeps.
    struct WaitRpc {
        notif: RefCell<std::collections::VecDeque<Result<Option<Notification>, RpcError>>>,
    }
    impl crate::provider::codex::AppServerRpc for WaitRpc {
        fn initialize(&self, _c: &ClientInfo) -> Result<InitializeResult, RpcError> {
            Ok(InitializeResult::default())
        }
        fn initialized(&self) -> Result<(), RpcError> {
            Ok(())
        }
        fn thread_start(&self, _c: &str, _a: &str, _s: &str) -> Result<String, RpcError> {
            unreachable!()
        }
        fn thread_resume(&self, _id: &str) -> Result<(), RpcError> {
            unreachable!()
        }
        fn turn_start(&self, _t: &str, _x: &str) -> Result<String, RpcError> {
            unreachable!()
        }
        fn turn_steer(&self, _t: &str, _e: &str, _x: &str) -> Result<SteerOutcome, RpcError> {
            unreachable!()
        }
        fn turn_interrupt(&self, _t: &str, _u: &str) -> Result<(), RpcError> {
            unreachable!()
        }
        fn next_notification(
            &self,
            _t: std::time::Duration,
        ) -> Result<Option<Notification>, RpcError> {
            self.notif.borrow_mut().pop_front().unwrap_or(Ok(None))
        }
        fn close(&self) -> Result<(), RpcError> {
            Ok(())
        }
    }

    fn idle_notif() -> Notification {
        Notification {
            method: "thread/status/changed".to_string(),
            params: serde_json::json!({"threadId":"T","status":{"type":"idle"}}),
        }
    }
    fn active_notif() -> Notification {
        Notification {
            method: "thread/status/changed".to_string(),
            params: serde_json::json!({"threadId":"T","status":{"type":"active","activeFlags":[]}}),
        }
    }

    #[test]
    fn real_deps_live_idle_notification_is_done() {
        use crate::boot::RealSleeper;

        let rpc = WaitRpc {
            notif: RefCell::new(
                [Ok(Some(active_notif())), Ok(Some(idle_notif()))]
                    .into_iter()
                    .collect(),
            ),
        };
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealCodexWaitDeps::new(
            Some(&rpc),
            None, // no rollout needed — the live idle resolves it
            std::time::Duration::from_millis(1),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_codex_wait_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn real_deps_daemon_unreachable_falls_back_to_rollout_idle() {
        // The daemon dropped (Closed) → the deps LATCH the rollout-tail channel and
        // resolve from a BALANCED rollout (derive_status → Idle). This is the
        // codex-p2-spec section 7.6 daemon-unreachable fallback.
        use crate::boot::RealSleeper;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        // A balanced rollout (started + completed) → derive_status Idle.
        std::fs::write(
            &path,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"A\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"A\"}}\n",
        )
        .unwrap();
        let rpc = WaitRpc {
            // First poll: the connection is closed → fall back to the rollout tail.
            notif: RefCell::new([Err(RpcError::Closed)].into_iter().collect()),
        };
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealCodexWaitDeps::new(
            Some(&rpc),
            Some(path),
            std::time::Duration::from_millis(1),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_codex_wait_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "daemon-unreachable falls back to the rollout tail and resolves Idle"
        );
    }

    #[test]
    fn real_deps_no_connect_starts_on_rollout_fallback() {
        // rpc None (connect failed) → the deps start fell-back; a balanced rollout
        // resolves to Idle WITHOUT ever touching a socket.
        use crate::boot::RealSleeper;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"A\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"A\"}}\n",
        )
        .unwrap();
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealCodexWaitDeps::new(
            None,
            Some(path),
            std::time::Duration::from_millis(1),
            &clock,
            &sleeper,
        );
        assert_eq!(
            run_codex_wait_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done
        );
    }

    #[test]
    fn real_deps_quiet_socket_idle_tail_resolves_done() {
        // W9 FIX Mo-1 (MUTATION EVIDENCE): the live channel is CONNECTED and the
        // socket is QUIET (next_notification → Ok(None), NO idle broadcast — the
        // thread went idle in the connect window so the broadcast is already past),
        // but the durable rollout tail is BALANCED (Idle). The fix cross-checks the
        // tail on a quiet poll and resolves Done.
        //
        // Without the Mo-1 tail cross-check, `Ok(None)` maps to Pending and the loop
        // polls to the timeout on an actually-idle thread (→ Timeout, not Done) —
        // this test reds if the cross-check is removed.
        use crate::boot::RealSleeper;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        // A balanced rollout (started + completed) → derive_status Idle.
        std::fs::write(
            &path,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"A\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"A\"}}\n",
        )
        .unwrap();
        // The rpc is CONNECTED and ALIVE but quiet: every next_notification → Ok(None)
        // (the WaitRpc default when its queue is empty). NO Closed/Transport drop —
        // so the deps do NOT fall back; the tail peek on the LIVE channel is what
        // resolves it.
        let rpc = WaitRpc {
            notif: RefCell::new(std::collections::VecDeque::new()), // empty → always Ok(None)
        };
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealCodexWaitDeps::new(
            Some(&rpc),
            Some(path),
            std::time::Duration::from_millis(1),
            &clock,
            &sleeper,
        );
        // The deps must NOT have fallen back (the socket never dropped); the quiet
        // poll's tail cross-check resolves Done.
        assert!(!deps.fell_back.get(), "the live socket never dropped");
        assert_eq!(
            run_codex_wait_loop(&deps, 60_000, 0),
            WaitStatusOutcome::Done,
            "a quiet-but-alive socket + a balanced rollout tail resolves Done (Mo-1)"
        );
        // Still on the live channel (the tail peek does NOT latch the fallback).
        assert!(
            !deps.fell_back.get(),
            "a quiet-poll tail peek must NOT latch the rollout-only fallback"
        );
    }

    #[test]
    fn real_deps_quiet_socket_busy_tail_keeps_polling() {
        // Mo-1 negative control: a quiet socket + a BUSY rollout tail (an open turn)
        // must KEEP POLLING (not Done) — the tail cross-check only resolves on an
        // IDLE tail, never falsely completes a genuinely-running turn.
        use crate::boot::RealSleeper;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        // An OPEN turn (started, no complete) → derive_status Busy.
        std::fs::write(
            &path,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"A\"}}\n",
        )
        .unwrap();
        let rpc = WaitRpc {
            notif: RefCell::new(std::collections::VecDeque::new()),
        };
        let clock = StepClock::new(1000);
        let sleeper = RealSleeper;
        let deps = RealCodexWaitDeps::new(
            Some(&rpc),
            Some(path),
            std::time::Duration::from_millis(1),
            &clock,
            &sleeper,
        );
        // A short timeout: a busy tail must not resolve → Timeout.
        assert_eq!(
            run_codex_wait_loop(&deps, 1500, 0),
            WaitStatusOutcome::Timeout,
            "a quiet socket + a BUSY tail keeps polling (no false Done)"
        );
    }
}
