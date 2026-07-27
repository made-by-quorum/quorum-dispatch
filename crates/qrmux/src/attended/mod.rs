//! attended/ — the M1 "polite core" state machine (attended-UX, qd-hardening).
//!
//! This module owns the *pure, clock-injected* machinery the qrmux mux server needs
//! to deliver a send POLITELY into a session a human is attending: the input
//! **journal** (P1), the input **lock** (P3), the journal-dependent **timer**
//! decision (P2), and the restart **reconciliation** decision (P4/RT-R1). The
//! byte-identical terminal **emitter** lives in [`emitter`]; the durable
//! pending-delivery **spool** lives in [`spool`].
//!
//! # Why pure + clock-injected
//! The correctness-critical behavior here is races, locks, timers, and crash
//! reconciliation. Making the logic a pure state machine over an injected clock
//! (never the wall clock, never real sleeps) lets the deterministic gates drive
//! exact interleavings — the fakerepl philosophy (zero RNG, injected knobs). The
//! live wiring (the per-session tokio timer task, the `client_to_pty` Input hook,
//! the fire sequence over the real PTY via the extracted `quorum_submit_discipline`)
//! is the thin integration layer that drives THIS logic.
//!
//! # Attendance is OBSERVED, never guessed
//! Human keystrokes reach this state machine ONLY via [`Journal::on_human_input`],
//! which the server calls from the attach-scoped `ClientMsg::Input` relay arm
//! (`session_relay.rs`) — a path that exists only while a client is attached.
//! qd's injection (`ClientMsg::SendInput`) and the fire's own replay bytes take a
//! different path and MUST NOT call `on_human_input`, so injected/replayed bytes
//! never enter the journal.

pub mod banner;
pub mod driver;
pub mod emitter;
pub mod fire;
pub mod spool;
pub mod sweep;

#[cfg(test)]
mod tests_wired;

// ===========================================================================
// Injected clock (the pure state machine never reads the wall clock)
// ===========================================================================

/// Monotonic-enough epoch-ms clock, injected so tests drive exact timings.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Production clock: `SystemTime::now()` epoch ms.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

// ===========================================================================
// Tunable timing (gate item 4 — defaults; Pete leans GENEROUS)
// ===========================================================================

/// Timing knobs. Changeable without replan (gate item 4). Defaults err toward a
/// longer quiet window / more generous immediate-send (Pete's steer, BUILD-
/// DIRECTIVES §1(3)).
#[derive(Clone, Copy, Debug)]
pub struct AttendedConfig {
    /// Countdown ceiling from send-acceptance (ms). The hold NEVER exceeds this
    /// relative to acceptance, even under continuous typing. Default ~60s.
    pub countdown_ceiling_ms: i64,
    /// Shortened ceiling applied when the send carries a priority flag. Default
    /// ~10s. Must be `<= countdown_ceiling_ms`.
    pub priority_ceiling_ms: i64,
    /// Keyboard-quiet window (ms): the send fires once the keyboard has been quiet
    /// this long. Low single-digit seconds. Default 3s.
    pub quiet_window_ms: i64,
    /// Paste-burst threshold (bytes): a human `Input` burst >= this is a PASTE (its
    /// CRs are literal newlines, never a submit). Mirrors the submit-discipline /
    /// fakerepl burst model. Default 8.
    pub paste_threshold: usize,
}

impl Default for AttendedConfig {
    fn default() -> Self {
        Self {
            countdown_ceiling_ms: 60_000,
            priority_ceiling_ms: 10_000,
            quiet_window_ms: 3_000,
            paste_threshold: 8,
        }
    }
}

impl AttendedConfig {
    /// The effective ceiling for a send, honoring its priority flag.
    fn ceiling(&self, priority: bool) -> i64 {
        if priority {
            self.priority_ceiling_ms.min(self.countdown_ceiling_ms)
        } else {
            self.countdown_ceiling_ms
        }
    }
}

// ===========================================================================
// P1 — the input journal (attach-scoped human draft since last submit)
// ===========================================================================

/// The authoritative draft: the human's keystrokes since their last submit, as a
/// byte stream (NOT a screen scrape — so byte-exact restoration is inherent, incl.
/// wrapped/over-long/scroll-composed composers; QS-2).
#[derive(Debug, Default, Clone)]
pub struct Journal {
    draft: Vec<u8>,
    /// Epoch ms of the last human keystroke observed in the mux. `None` until a
    /// human has typed (a no-attach session never records one → quiet is satisfied
    /// immediately; QS-3).
    last_keystroke_ms: Option<i64>,
}

impl Journal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe an attach-scoped human input burst at `now_ms`. Records
    /// `last_keystroke_ms`, appends content to the draft, and rolls the draft over
    /// on an observed human SUBMIT (a lone non-paste CR keystroke on a non-empty
    /// draft — the fakerepl/claude burst model). Injected/replayed bytes must NEVER
    /// reach this method.
    ///
    /// Returns `true` if this burst rolled the draft (a human submit boundary was
    /// crossed) — the caller uses it only for observability.
    pub fn on_human_input(&mut self, bytes: &[u8], now_ms: i64, paste_threshold: usize) -> bool {
        self.last_keystroke_ms = Some(now_ms);
        let is_paste = bytes.len() >= paste_threshold;
        let mut rolled = false;
        for &b in bytes {
            if b == b'\r' {
                if is_paste {
                    // Paste-burst CR → literal newline into the draft (claude's
                    // paste behavior); NOT a submit.
                    self.draft.push(b'\n');
                } else if self.draft.is_empty() {
                    // Lone CR on an empty composer submits nothing (claude starts no
                    // turn for an empty prompt) — no roll, no content.
                } else {
                    // Lone non-paste CR keystroke on a non-empty draft → the human
                    // submitted their own turn. Roll: the draft up to here left as
                    // the human's turn; a fresh draft begins.
                    self.draft.clear();
                    rolled = true;
                }
            } else {
                self.draft.push(b);
            }
        }
        rolled
    }

    /// Is the current draft empty (no un-submitted human words)?
    pub fn is_empty(&self) -> bool {
        self.draft.is_empty()
    }

    /// The current draft bytes.
    pub fn draft(&self) -> &[u8] {
        &self.draft
    }

    /// Epoch ms of the last human keystroke, if any.
    pub fn last_keystroke_ms(&self) -> Option<i64> {
        self.last_keystroke_ms
    }

    /// A byte-exact snapshot of the current draft (for the durable spool).
    pub fn snapshot(&self) -> Vec<u8> {
        self.draft.clone()
    }
}

/// Backward-looking quiet check (RT-R3, BINDING). `now − last_keystroke ≥ window`.
/// A session that never recorded a keystroke (no attach) is quiet IMMEDIATELY.
/// This is a point-in-time check — NEVER a forward sleep, NEVER a send-path delay.
pub fn quiet_satisfied(last_keystroke_ms: Option<i64>, now_ms: i64, window_ms: i64) -> bool {
    match last_keystroke_ms {
        None => true,
        Some(t) => now_ms - t >= window_ms,
    }
}

// ===========================================================================
// P3 — the input lock (buffer human input during a fire; ordered, never drops)
// ===========================================================================

/// While a fire is in progress the lock buffers human input in order so it never
/// enters the PTY mid-fire (closing the fires-while-typing race, QS-1) and is never
/// dropped. Flushed in order on unlock.
#[derive(Debug, Default)]
pub struct InputLock {
    locked: bool,
    buffer: Vec<u8>,
}

/// What the caller should do with a human input burst while the lock may be held.
#[derive(Debug, PartialEq, Eq)]
pub enum Admit {
    /// Not locked — write these bytes straight to the PTY (normal path).
    Passthrough(Vec<u8>),
    /// Locked — the bytes were buffered in order; write NOTHING to the PTY now.
    Buffered,
}

impl InputLock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Arm the lock (fire start). Idempotent.
    pub fn lock(&mut self) {
        self.locked = true;
    }

    /// Offer a human input burst. While locked, buffers it (ordered, never
    /// dropped) and returns [`Admit::Buffered`]; otherwise returns
    /// [`Admit::Passthrough`] for the caller to write to the PTY.
    pub fn admit(&mut self, bytes: &[u8]) -> Admit {
        if self.locked {
            self.buffer.extend_from_slice(bytes);
            Admit::Buffered
        } else {
            Admit::Passthrough(bytes.to_vec())
        }
    }

    /// Release the lock and drain the buffered bytes IN ORDER (fire end → flush).
    pub fn unlock_and_drain(&mut self) -> Vec<u8> {
        self.locked = false;
        std::mem::take(&mut self.buffer)
    }
}

// ===========================================================================
// P2 — the journal-dependent timer DECISION (backward quiet, keystroke-reset,
// ceiling-bounded). Pure — the per-session tokio timer task drives it.
// ===========================================================================

/// What the timer should do for a send, evaluated at a point in time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FireDecision {
    /// Fire the send now (immediate — empty journal + quiet, or ceiling reached,
    /// or quiet elapsed, or deliver-now).
    Immediate,
    /// Hold: (re)arm the countdown to wake at `deadline_ms`. The task sleeps until
    /// then (resettable on each keystroke) and re-evaluates on wake.
    Countdown { deadline_ms: i64 },
}

/// Evaluate the fire decision at `now_ms` for a send accepted at `accepted_at_ms`.
///
/// - **Empty journal + backward-quiet ⇒ Immediate** (the fast path: nothing to
///   preserve, keyboard quiet — the unattended/overnight case is Immediate by
///   construction because a no-attach session's `last_keystroke_ms` is `None`).
/// - Otherwise **Countdown** to `deadline = min(last_keystroke + quiet_window,
///   accepted_at + ceiling)`: fire once the keyboard is quiet for `quiet_window`
///   (keystroke-relative bounded deferral, interpretation (i)), but never later
///   than `accepted_at + ceiling` even under continuous typing.
/// - When `now_ms >= deadline` the decision is Immediate (quiet elapsed or ceiling
///   reached) — the send fires, preserving+replaying whatever draft remains.
///
/// NEVER a forward sleep on the send path: `Immediate` fires now; `Countdown` only
/// arms because a non-empty draft (or recent typing) is actively deferring OUR send.
pub fn evaluate(
    journal: &Journal,
    accepted_at_ms: i64,
    now_ms: i64,
    cfg: &AttendedConfig,
    priority: bool,
) -> FireDecision {
    let ceiling_deadline = accepted_at_ms + cfg.ceiling(priority);
    // Ceiling reached ⇒ fire regardless of continued typing.
    if now_ms >= ceiling_deadline {
        return FireDecision::Immediate;
    }
    // Empty draft + quiet ⇒ immediate fast path.
    if journal.is_empty() && quiet_satisfied(journal.last_keystroke_ms, now_ms, cfg.quiet_window_ms)
    {
        return FireDecision::Immediate;
    }
    // Quiet elapsed since the last keystroke ⇒ fire (preserving any draft).
    if quiet_satisfied(journal.last_keystroke_ms, now_ms, cfg.quiet_window_ms) {
        return FireDecision::Immediate;
    }
    // Still within the quiet window and under the ceiling ⇒ hold. Deadline is the
    // earlier of (quiet elapses) and (ceiling). `last_keystroke_ms` is Some here
    // (a non-quiet state implies a recent keystroke).
    let quiet_deadline = journal
        .last_keystroke_ms
        .map(|t| t + cfg.quiet_window_ms)
        .unwrap_or(now_ms);
    FireDecision::Countdown {
        deadline_ms: quiet_deadline.min(ceiling_deadline),
    }
}

// ===========================================================================
// P4 / RT-R1 — restart reconciliation DECISION (pure; keyed to the observed
// PTY-survival fact: the hosted session DIES with the mux)
// ===========================================================================

/// A spooled send's durable fire progress (see [`spool`]).
#[derive(Debug, PartialEq, Eq, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum FirePhase {
    /// Write-ahead record at acceptance; fire not started.
    Accepted,
    /// Countdown armed; draft snapshot durable; fire not started.
    Countdown,
    /// `fire_started` durable BEFORE the clear-chord — inject MAY have run.
    FireStarted,
    /// `fire_completed` durable — inject provably ran to completion.
    FireCompleted,
}

/// The result of a landing probe over the transcript window (message-seen shape).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LandingResult {
    /// The payload is confirmed landed (sha match past the pre-fire offset).
    Landed,
    /// A shorter-with-shared-prefix candidate landed (truncation).
    Mismatch,
    /// No landing could be confirmed (no candidate / unreadable / unattributable).
    Unconfirmed,
}

/// The honest terminal a restart reconciliation must emit for a spooled send.
/// Keyed to the observed fact that the hosted session DIES with the mux, so
/// `session_alive` is `false` in production on this box (the live-session banner
/// re-surface path is retained for completeness).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ReconcileVerdict {
    /// A terminal already exists in the ledger (first-terminal-wins) — skip.
    AlreadyTerminal,
    /// Inject provably ran and the payload is confirmed landed → late message-seen.
    LateMessageSeen,
    /// Inject ran, transcript shows a truncated landing → turn-anchored-mismatch.
    LateMismatch,
    /// Inject provably did NOT run; session gone → honest failed terminal, draft
    /// retained-and-reported loudly. NEVER re-inject.
    SeenFailedRecipientGone,
    /// Unknown inject outcome (crash mid-fire) that cannot be confirmed landed →
    /// honest pending-abandoned. NEVER re-inject (no delivered-that-wasn't).
    PendingAbandonedUnknown,
    /// Inject provably did NOT run and the session is (unexpectedly) alive → the
    /// draft re-surfaces via the banner recovery path; no terminal yet.
    ResurfaceDraft,
}

/// Reconcile a single spooled send to EXACTLY ONE honest terminal (or a skip /
/// resurface). `already_terminal` short-circuits (first-terminal-wins, checked
/// under the ledger flock by the caller). Rules per hard point 2 / RT-R1.
pub fn reconcile(
    phase: FirePhase,
    landing: LandingResult,
    session_alive: bool,
    already_terminal: bool,
) -> ReconcileVerdict {
    if already_terminal {
        return ReconcileVerdict::AlreadyTerminal;
    }
    match phase {
        // Inject provably did not run.
        FirePhase::Accepted | FirePhase::Countdown => {
            if session_alive {
                ReconcileVerdict::ResurfaceDraft
            } else {
                ReconcileVerdict::SeenFailedRecipientGone
            }
        }
        // Inject provably ran to completion → confirm landing, never re-inject.
        FirePhase::FireCompleted => match landing {
            LandingResult::Landed => ReconcileVerdict::LateMessageSeen,
            LandingResult::Mismatch => ReconcileVerdict::LateMismatch,
            LandingResult::Unconfirmed => ReconcileVerdict::PendingAbandonedUnknown,
        },
        // Unknown inject outcome (crash mid-fire). NEVER re-inject. Emit success
        // ONLY if the transcript independently proves it landed.
        FirePhase::FireStarted => match landing {
            LandingResult::Landed => ReconcileVerdict::LateMessageSeen,
            LandingResult::Mismatch => ReconcileVerdict::LateMismatch,
            LandingResult::Unconfirmed => ReconcileVerdict::PendingAbandonedUnknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: usize = 8; // paste threshold

    // ---- Journal (P1) --------------------------------------------------------

    #[test]
    fn journal_accumulates_human_keystrokes_bytewise() {
        let mut j = Journal::new();
        j.on_human_input(b"hel", 100, T);
        j.on_human_input(b"lo", 200, T);
        assert_eq!(j.draft(), b"hello");
        assert_eq!(j.last_keystroke_ms(), Some(200));
        assert!(!j.is_empty());
    }

    #[test]
    fn journal_lone_cr_keystroke_rolls_the_draft() {
        let mut j = Journal::new();
        j.on_human_input(b"hi", 100, T);
        // A lone CR keystroke (non-paste, len 1 < threshold) on a non-empty draft
        // is the human's own submit → roll to empty.
        let rolled = j.on_human_input(b"\r", 200, T);
        assert!(rolled);
        assert!(j.is_empty());
        // A fresh draft accumulates after the roll.
        j.on_human_input(b"next", 300, T);
        assert_eq!(j.draft(), b"next");
    }

    #[test]
    fn journal_paste_burst_cr_is_literal_newline_not_submit() {
        let mut j = Journal::new();
        // A >=8-byte burst containing a CR is a paste → CR is a literal newline,
        // never a submit; the draft keeps growing (multi-line paste).
        let rolled = j.on_human_input(b"line1\rline2", 100, T);
        assert!(!rolled);
        assert_eq!(j.draft(), b"line1\nline2");
    }

    #[test]
    fn journal_lone_cr_on_empty_draft_is_noop() {
        let mut j = Journal::new();
        let rolled = j.on_human_input(b"\r", 100, T);
        assert!(!rolled);
        assert!(j.is_empty());
        // But it DID record a keystroke time (the human pressed enter).
        assert_eq!(j.last_keystroke_ms(), Some(100));
    }

    #[test]
    fn journal_snapshot_is_byte_exact_over_wrapped_overlong_draft() {
        // QS-2: the journal is the byte stream, independent of screen wrap. A long
        // multi-line draft round-trips byte-for-byte through snapshot.
        let mut j = Journal::new();
        let long: Vec<u8> = (0..5000).map(|i| b'a' + (i % 26) as u8).collect();
        j.on_human_input(&long, 100, T); // one paste burst, no CR
        assert_eq!(j.snapshot(), long);
        assert_eq!(j.snapshot().len(), 5000);
    }

    // ---- Quiet check (P2/P4, RT-R3) -----------------------------------------

    #[test]
    fn quiet_no_keystroke_is_immediately_satisfied() {
        // No attach / no keystroke ever → quiet immediately (the unattended
        // guarantee; QS-3 non-regression).
        assert!(quiet_satisfied(None, 999_999, 3_000));
    }

    #[test]
    fn quiet_is_a_backward_check_never_a_forward_sleep() {
        // now - last >= window.
        assert!(!quiet_satisfied(Some(1_000), 1_001, 3_000)); // 1ms elapsed, not quiet
        assert!(!quiet_satisfied(Some(1_000), 3_999, 3_000)); // 2999ms, not yet
        assert!(quiet_satisfied(Some(1_000), 4_000, 3_000)); // exactly 3000ms → quiet
        assert!(quiet_satisfied(Some(1_000), 9_000, 3_000));
    }

    // ---- Input lock (P3, QS-1) ----------------------------------------------

    #[test]
    fn lock_buffers_input_in_order_and_never_drops_or_leaks_to_pty() {
        let mut lk = InputLock::new();
        // Unlocked → passthrough.
        assert_eq!(lk.admit(b"a"), Admit::Passthrough(b"a".to_vec()));
        // Fire starts → lock.
        lk.lock();
        assert!(lk.is_locked());
        // Keystrokes during the fire buffer in order, NOTHING to the PTY.
        assert_eq!(lk.admit(b"bc"), Admit::Buffered);
        assert_eq!(lk.admit(b"d"), Admit::Buffered);
        // Fire ends → drain in order.
        assert_eq!(lk.unlock_and_drain(), b"bcd".to_vec());
        assert!(!lk.is_locked());
        // After unlock, passthrough resumes.
        assert_eq!(lk.admit(b"e"), Admit::Passthrough(b"e".to_vec()));
    }

    #[test]
    fn lock_keystroke_timed_into_the_fire_window_is_preserved_not_lost() {
        // QS-1: a keystroke arriving exactly during the lock window is buffered and
        // flushed, never lost or interleaved into the injected bytes.
        let mut lk = InputLock::new();
        lk.lock();
        for chunk in [b"x".as_slice(), b"y".as_slice(), b"z".as_slice()] {
            assert_eq!(lk.admit(chunk), Admit::Buffered);
        }
        assert_eq!(lk.unlock_and_drain(), b"xyz".to_vec());
    }

    // ---- Timer decision (P2, interpretation (i)/(iii)) ----------------------

    fn cfg() -> AttendedConfig {
        AttendedConfig {
            countdown_ceiling_ms: 60_000,
            priority_ceiling_ms: 10_000,
            quiet_window_ms: 3_000,
            paste_threshold: 8,
        }
    }

    #[test]
    fn timer_empty_journal_quiet_fires_immediately() {
        let j = Journal::new(); // empty, no keystroke
        assert_eq!(evaluate(&j, 0, 0, &cfg(), false), FireDecision::Immediate);
        // Unattended overnight case: still immediate long after acceptance.
        assert_eq!(
            evaluate(&j, 0, 50_000, &cfg(), false),
            FireDecision::Immediate
        );
    }

    #[test]
    fn timer_nonempty_draft_holds_then_fires_when_quiet() {
        let mut j = Journal::new();
        j.on_human_input(b"typing", 1_000, T); // last_keystroke = 1000
        let accepted = 1_000;
        // 1ms after the keystroke: hold, deadline = min(1000+3000, 1000+60000)=4000.
        assert_eq!(
            evaluate(&j, accepted, 1_001, &cfg(), false),
            FireDecision::Countdown { deadline_ms: 4_000 }
        );
        // At the quiet deadline: fire.
        assert_eq!(
            evaluate(&j, accepted, 4_000, &cfg(), false),
            FireDecision::Immediate
        );
    }

    #[test]
    fn timer_keystroke_reset_extends_hold_up_to_the_ceiling() {
        let mut j = Journal::new();
        j.on_human_input(b"a", 1_000, T);
        let accepted = 1_000;
        // Keystroke resets the quiet clock: at t=2000 a new keystroke pushes the
        // deadline to 2000+3000=5000.
        j.on_human_input(b"b", 2_000, T);
        assert_eq!(
            evaluate(&j, accepted, 2_001, &cfg(), false),
            FireDecision::Countdown { deadline_ms: 5_000 }
        );
        // Continuous typing never pushes past the ceiling (accepted+60000=61000).
        j.on_human_input(b"c", 60_500, T);
        // Past the ceiling → fire regardless of the fresh keystroke.
        assert_eq!(
            evaluate(&j, accepted, 61_000, &cfg(), false),
            FireDecision::Immediate
        );
    }

    #[test]
    fn timer_priority_shortens_the_ceiling() {
        let mut j = Journal::new();
        j.on_human_input(b"draft", 0, T);
        // Continuous typing; priority ceiling is 10s.
        j.on_human_input(b"x", 10_000, T);
        assert_eq!(
            evaluate(&j, 0, 10_000, &cfg(), true),
            FireDecision::Immediate // ceiling (0+10000) reached
        );
        // Without priority, still holding at t=10000 (ceiling 60000).
        assert!(matches!(
            evaluate(&j, 0, 10_001, &cfg(), false),
            FireDecision::Countdown { .. }
        ));
    }

    // ---- Reconciliation (P4/RT-R1, keyed to session-dies) -------------------

    #[test]
    fn reconcile_already_terminal_skips() {
        assert_eq!(
            reconcile(FirePhase::FireStarted, LandingResult::Landed, false, true),
            ReconcileVerdict::AlreadyTerminal
        );
    }

    #[test]
    fn reconcile_inject_not_started_session_gone_fails_honestly_never_reinjects() {
        for phase in [FirePhase::Accepted, FirePhase::Countdown] {
            assert_eq!(
                reconcile(phase, LandingResult::Unconfirmed, false, false),
                ReconcileVerdict::SeenFailedRecipientGone
            );
        }
    }

    #[test]
    fn reconcile_inject_not_started_session_alive_resurfaces_draft() {
        assert_eq!(
            reconcile(FirePhase::Countdown, LandingResult::Unconfirmed, true, false),
            ReconcileVerdict::ResurfaceDraft
        );
    }

    #[test]
    fn reconcile_completed_landed_is_late_message_seen() {
        assert_eq!(
            reconcile(FirePhase::FireCompleted, LandingResult::Landed, false, false),
            ReconcileVerdict::LateMessageSeen
        );
    }

    #[test]
    fn reconcile_unknown_inject_never_claims_landed_without_proof() {
        // FireStarted (crash mid-fire) + unconfirmed landing → honest abandoned,
        // NEVER a success terminal (no delivered-that-wasn't; RT-R1).
        assert_eq!(
            reconcile(FirePhase::FireStarted, LandingResult::Unconfirmed, false, false),
            ReconcileVerdict::PendingAbandonedUnknown
        );
        // But if the transcript INDEPENDENTLY proves it landed, late message-seen.
        assert_eq!(
            reconcile(FirePhase::FireStarted, LandingResult::Landed, false, false),
            ReconcileVerdict::LateMessageSeen
        );
    }
}
