//! attended/fire.rs — the fire-sequence engine (P3), the constants seam (M4
//! fills), and the landing probe (RT-R2 message-seen).
//!
//! The fire sequence drives a spooled pending send onto the real PTY POLITELY:
//!
//! ```text
//! lock → plain-composer verify → fire-start durable write (BEFORE clear) →
//! clear-chord → inject (shared submit discipline) → replay journal draft →
//! unlock + flush buffered input → bounded landing verify → terminal
//! ```
//!
//! # Why this shape (the binding clauses)
//! - **QS-1** — the [`InputLock`](super::InputLock) is armed for the WHOLE fire, so
//!   human keystrokes that arrive mid-fire buffer in order and flush on unlock;
//!   they never interleave with the injected bytes and are never dropped.
//! - **QS-5** — the plain-composer verify GATES the clear-chord/inject: a composer
//!   that is not verifiably plain (a modal/palette, or missing harness facts) after
//!   bounded retry is an honest `send-failed{verify-blocked}`, NEVER a blind type.
//!   The inject's own CR is CONTENT-VERIFIED by the discipline
//!   ([`composer_holds_message`](quorum_submit_discipline::composer_holds_message)) —
//!   it submits only while the composer provably still holds our exact text.
//! - **QS-4 / RT-R1** — `fire_started` is durable BEFORE the first clear-chord byte,
//!   so a crash mid-fire reconciles as "inject MAY have run" (never re-injects);
//!   `fire_completed` is durable after a confirmed accept. Every fire-step failure
//!   re-shows the preserved draft.
//! - **Terminal honesty (the [FORK], QS-6/QS-7)** — a success terminal
//!   (`message-seen`) requires a bounded post-fire LANDING verify (the payload is
//!   confirmed in the hosted transcript), matching today's no-wait `send:pty`
//!   semantics. Mere acceptance is NOT terminal: an injected+accepted send whose
//!   landing is not yet confirmable stays [`FireOutcome::Pending`] (the record stays
//!   spooled at `FireCompleted`; `--wait`/reconcile resolves it) — never a false
//!   "landed".
//!
//! # Constants seam (M4 fills; wrong/missing facts degrade safely)
//! [`HarnessFacts`] injects the per-harness clear-chord, composer region, and
//! plain-composer predicate. M1 ships [`SafeDefaultFacts`]; M4 supplies real
//! per-harness facts (codex, pi). Wrong or missing facts degrade to the
//! plain-composer verify + P4 re-show, NEVER to blind typing (the content-verified
//! CR is the backstop when a fact is wrong).
//!
//! # Acceptance-confirmable gate (M4 F1)
//! A harness with NO confirmable turn-acceptance signal
//! ([`FireEffects::acceptance_confirmable`] `== false` — codex/pi, whose busy/idle
//! status source is a Q7 residual) is gated OFF at the TOP of [`fire`], before any
//! lock/clear/inject: the send resolves to an honest non-delivery terminal with the
//! composer UNTOUCHED. Firing there would inject a real turn we can never observe
//! accepted and then report the real delivery as a failure (the F1 false-negative +
//! double-submit hazard). It is the STATUS-fact analog of the composer-fact gate.

use quorum_delivery_events::Payload;
use quorum_submit_discipline::{
    deliver_idle_two_write_in_region, send_text_chunked, ChunkSendOptions, ComposerRegion,
    IdleDeliverDeps, SubmitOptions, PROMPT_GLYPH, TWO_WRITE_SETTLE_MS,
};
use std::sync::Mutex;

use super::spool::{PendingRecord, Spool};
use super::{FirePhase, Journal, InputLock, LandingResult};

// ===========================================================================
// FireEffects — the live effects the fire drives (the qrmux-side "Deps"). The
// real binding writes to the session's PTY writer + reads its screen model +
// child status; tests inject a fake. Blocking by construction (the discipline is
// synchronous) — the caller runs `fire` in `spawn_blocking`.
// ===========================================================================

/// The live effects the fire sequence needs. All blocking (the submit discipline
/// is synchronous). The real binding is [`crate::server`]-side; tests inject a
/// scripted fake.
pub trait FireEffects: Send + Sync {
    /// Raw PTY write of `text` ALONE (no CR) — the discipline's chunked text
    /// write. Infallible in the trait (a write error surfaces as "not accepted").
    fn send_text(&self, text: &str);
    /// Raw PTY write of a lone `"\r"` — the discipline's separate/remediation CR.
    fn send_cr(&self);
    /// Raw PTY write of arbitrary bytes — the clear-chord, the journal replay, and
    /// the buffered-input flush. FALLIBLE: a clear-chord write error is an honest
    /// `inject-failed` (we detect it BEFORE trusting the composer state).
    fn write_raw(&self, bytes: &[u8]) -> std::io::Result<()>;
    /// The session's rendered screen text (qrmux screen model), for the
    /// plain-composer verify + the content-verified CR predicate.
    fn read_screen(&self) -> String;
    /// The hosted child's status (`Some("busy")`/`Some("idle")`), or `None` when
    /// the status source is gone (the session likely died — recipient-gone).
    fn read_status(&self) -> Option<String>;
    /// Whether this harness has ANY confirmable turn-acceptance signal (M4 F1) — a
    /// STANDING property of the status source, NOT a momentary [`read_status`]
    /// value. `false` ⇒ the fire is gated OFF before any clear/inject: injecting a
    /// real turn we can never observe accepted would report a real delivery as a
    /// failure (F1), and `composer_is_plain` alone cannot rule out a busy composer.
    /// claude (`claude_default`) is `true`; codex/pi (`none_source`, Q7 busy-state
    /// residual) are `false`.
    fn acceptance_confirmable(&self) -> bool;
    /// Sleep `ms` (injected so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic-ish clock in ms (injected).
    fn now_ms(&self) -> i64;
}

/// Adapts a `&dyn FireEffects` into an [`IdleDeliverDeps`] so the shared
/// `deliver_idle_two_write` discipline drives the SAME writes.
struct AsIdle<'a>(&'a dyn FireEffects);
impl IdleDeliverDeps for AsIdle<'_> {
    fn send_text(&self, text: &str) {
        self.0.send_text(text)
    }
    fn send_cr(&self) {
        self.0.send_cr()
    }
    fn read_screen(&self) -> String {
        self.0.read_screen()
    }
    fn read_status(&self) -> Option<String> {
        self.0.read_status()
    }
    fn sleep(&self, ms: u64) {
        self.0.sleep(ms)
    }
    fn now_ms(&self) -> i64 {
        self.0.now_ms()
    }
}

// ===========================================================================
// HarnessFacts — the per-harness constants seam (M4 fills).
// ===========================================================================

/// Per-harness facts the fire needs, injected per session. M1 ships
/// [`SafeDefaultFacts`]; M4 supplies real per-harness impls (codex, pi). The
/// contract: a fact this seam cannot supply degrades the fire to an HONEST
/// failure + re-show, never a blind type.
pub trait HarnessFacts: Send + Sync {
    /// The clear-chord bytes that empty the composer (e.g. Ctrl-U on a
    /// readline-style composer). M4 supplies the real per-harness chord. The base
    /// chord for the default [`ClearStrategy`]; a harness that needs a richer
    /// clear (repeat / spacing) overrides [`HarnessFacts::clear_strategy`].
    fn clear_chord(&self) -> Vec<u8>;
    /// Is the rendered `screen_text` composer in a PLAIN state (safe to
    /// clear/type into — NOT a modal/palette/menu)?
    ///
    /// - `Some(true)` — plain, proceed.
    /// - `Some(false)` — provably NOT plain (a modal) → honest `verify-blocked`.
    /// - `None` — UNKNOWN (missing facts) → honest `verify-blocked`, never a
    ///   blind type. M4 turns `None`s into `Some(_)` per harness.
    fn composer_is_plain(&self, screen_text: &str) -> Option<bool>;

    /// HOW this harness's turn acceptance is confirmed (M5/T6).
    ///
    /// Default [`AcceptanceSignal::BusyTransition`]: the session publishes a
    /// pollable busy/idle status and going busy after the CR proves the turn
    /// started (claude, via its registry row). codex and pi override to
    /// [`AcceptanceSignal::Landing`] — neither publishes such a status, but both
    /// record the user's message in their transcript, so the LANDING is the
    /// acceptance proof.
    fn acceptance_signal(&self) -> AcceptanceSignal {
        AcceptanceSignal::BusyTransition
    }

    /// The composer region the content-verified CR anchors on (M4 — the
    /// generalization of the ❯-only anchor). Default: the claude glyph anchor
    /// `❯`. codex overrides `GlyphAnchor('›')`; pi (no glyph) overrides
    /// `BetweenLastTwo('─')`.
    fn composer_region(&self) -> ComposerRegion {
        ComposerRegion::GlyphAnchor(PROMPT_GLYPH)
    }

    /// How the fire's clear step drives the chord (M4 generalization of the fixed
    /// single-write `clear_chord`). Default: send `clear_chord()` exactly ONCE,
    /// unconditionally, no post-clear verify — byte-for-byte the M1-accepted claude
    /// behaviour. codex overrides a bounded, spaced, plain-verified repeat
    /// (repeated Ctrl-U converges); pi a single spaced Ctrl-C (a rapid second
    /// Ctrl-C EXITS pi, so `presses == 1`).
    fn clear_strategy(&self) -> ClearStrategy {
        ClearStrategy::once(self.clear_chord())
    }
}

/// What proves a harness ACCEPTED the turn (M5/T6 — the F1 un-gate).
///
/// The fire needs positive evidence that the message became a turn; the two
/// available proofs differ in strength, and only one is available per harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceSignal {
    /// The session goes BUSY after the CR (claude). Cheap and immediate, but only
    /// evidence that *something* started — the discipline pairs it with a
    /// content-verified remediation CR for the paste-absorbed case.
    BusyTransition,
    /// The message appears in the TRANSCRIPT as a user record (codex, pi).
    ///
    /// STRICTLY STRONGER evidence than a busy transition: it identifies the exact
    /// bytes that landed, so it cannot be fooled by unrelated activity, and — the
    /// reason codex needs it — it cannot be MISSED the way codex's on-screen
    /// `esc to interrupt` line can be, since streamed output overwrites that line
    /// mid-turn. It is also slower (the harness must flush the record), which is
    /// what `landing_window_ms` budgets for.
    ///
    /// A Landing harness takes a SINGLE CR with no busy-keyed remediation: the
    /// remediation exists to recover a CR the status signal says was absorbed, and
    /// with no status signal it would be an unconditional second CR — precisely
    /// the double-submit the F1 gate was protecting against.
    Landing,
}

/// How the fire's clear step drives the harness clear-chord (M4). The seam is a
/// strategy, not a fixed byte vector, because harnesses differ fundamentally:
/// codex has no single full-clear binding (repeated Ctrl-U converges, ~2L-1
/// presses for L composer lines, a safe no-op when already empty); pi has a
/// one-press Ctrl-C full clear whose RAPID repeat EXITS the harness.
///
/// Safe floor (QS-5): after the bounded presses, if `reverify_plain` and the
/// composer does not verify PLAIN, the fire DEGRADES to `verify-blocked` + P4
/// re-show — it NEVER blind-types into an unverified composer.
#[derive(Debug, Clone)]
pub struct ClearStrategy {
    /// The chord bytes to write on each press.
    pub chord: Vec<u8>,
    /// How many times to press the chord. `1` = a single press (claude default,
    /// pi's one-press-and-never-a-rapid-second Ctrl-C); `> 1` = a bounded repeat
    /// (codex's converge-by-repeated-Ctrl-U).
    pub presses: u32,
    /// Sleep this many ms after EACH press. Spaces a repeated chord (codex) and,
    /// critically, rate-limits so a chord is never re-sent faster than the
    /// harness tolerates. `0` = no inter-press sleep (the claude default).
    pub settle_ms: u64,
    /// Re-verify the composer is PLAIN after the bounded clear (QS-5 safe floor).
    /// `false` = the claude default (single unconditional press, byte-for-byte the
    /// M1 behaviour). `true` = codex/pi (degrade to `verify-blocked` if the clear
    /// left a non-plain composer, never a blind inject).
    pub reverify_plain: bool,
}

impl ClearStrategy {
    /// One unconditional press, no inter-press sleep, no post-clear verify — the
    /// M1-accepted claude/`SafeDefaultFacts` behaviour (keeps that path
    /// byte-for-byte).
    pub fn once(chord: Vec<u8>) -> Self {
        Self {
            chord,
            presses: 1,
            settle_ms: 0,
            reverify_plain: false,
        }
    }
}

/// Shared modal-signature discriminator (QS-5, O1). Returns `true` iff any
/// enumerated modal-chrome signature string is present on `screen_text`. Both
/// [`CodexFacts`] and [`SafeDefaultFacts`] consult this with their OWN
/// `modal_sigs` list so a modal whose selection marker COLLIDES with the
/// composer glyph — codex `›`, claude `❯` — is classified NOT-plain rather than
/// false-positived as a plain composer (the concrete blind-TYPE hazard). The
/// matching logic lives here ONCE (REUSE, don't fork); only the per-harness
/// signature data differs.
///
/// Non-exhaustive by nature (see the per-facts notes): it is defense hardening,
/// not the sole guard — a modal that REPLACES the composer glyph/status line
/// already fails the allowlist check below and degrades safe (`None`).
fn modal_screen_is_denied(screen_text: &str, modal_sigs: &[&str]) -> bool {
    modal_sigs.iter().any(|sig| screen_text.contains(sig))
}

/// M1's safe default facts (shipped until M4 lands per-harness facts). Best-effort
/// GENERIC composer model:
/// - clear-chord = `Ctrl-U` (0x15) — clears the line in readline/most composers;
/// - prompt glyph = `❯` (the discipline's [`PROMPT_GLYPH`]);
/// - `composer_is_plain` = `Some(false)` on a recognized modal (QS-5/O1 — see
///   [`CLAUDE_MODAL_SIGS`]); else `Some(true)` iff the prompt glyph is visible (a
///   composer anchor is present); else `None` (cannot locate a composer → fail safe).
///
/// This never claims to detect modals (that is M4's per-harness region work); the
/// safety against a wrong guess is the discipline's CONTENT-VERIFIED CR downstream
/// (it submits only while the composer holds our exact text), so a wrong clear or a
/// modal that happens to show a glyph degrades to an honest failure, not a blind
/// submit.
#[derive(Debug, Default, Clone, Copy)]
pub struct SafeDefaultFacts;

/// Ctrl-U — the generic clear-line chord (readline `unix-line-discard`).
const SAFE_CLEAR_CHORD: &[u8] = b"\x15";

/// claude `/model` modal signature strings (QS-5, O1). **PRIMARY SOURCE:** a
/// live claude 2.1.207 TUI driven to the `/model` dialog with its raw pane
/// captured (2026-07-13). The dialog KEEPS `❯` on screen — the composer line
/// PERSISTS as `❯ /model` AND the dialog's own selection marker is `❯ 6. Opus ✔`
/// — so glyph-presence ALONE false-positives the open modal as a plain composer,
/// and the fire would clear + TYPE into it (harm-bounded by the downstream
/// content-verified CR, but a real QS-5 letter gap). These header/subheader/
/// footer chrome strings (the claude analogues of codex's `Select Model and
/// Effort` / `Press enter to confirm or esc to go back`) let the shared
/// [`modal_screen_is_denied`] discriminator classify the modal NOT-plain.
const CLAUDE_MODAL_SIGS: &[&str] = &[
    "Select model",                  // /model dialog header
    "Switch between Claude models.", // /model dialog subheader (distinctive phrase)
    "Enter to set as default",       // /model dialog footer chrome (Enter/s/Esc row)
];

impl HarnessFacts for SafeDefaultFacts {
    fn clear_chord(&self) -> Vec<u8> {
        SAFE_CLEAR_CHORD.to_vec()
    }
    fn composer_is_plain(&self, screen_text: &str) -> Option<bool> {
        // QS-5 (O1): deny known modals FIRST via the SHARED discriminator (the
        // SAME mechanism CodexFacts uses). A claude `/model` modal keeps `❯` on
        // screen, so the glyph-presence check below would false-positive it as
        // plain and the fire could clear + TYPE into the modal. Classifying it
        // Some(false) makes the fire decline. See [`CLAUDE_MODAL_SIGS`].
        if modal_screen_is_denied(screen_text, CLAUDE_MODAL_SIGS) {
            return Some(false);
        }
        if screen_text.contains(PROMPT_GLYPH) {
            Some(true)
        } else {
            None
        }
    }
    // composer_region() and clear_strategy() use the trait defaults:
    // GlyphAnchor(❯) and a single unconditional Ctrl-U — the claude behaviour M1
    // is accepted on, kept byte-for-byte.
}

// ===========================================================================
// CodexFacts — codex-cli 0.144.1 (Q7 note 01KXAPXNH0, verified at source).
// ===========================================================================

/// codex's composer glyph `›` U+203A (UTF-8 `e2 80 ba`) — NOT `❯` U+276F. The
/// region-anchor on this glyph restores exact claude-equivalent semantics (a
/// submitted turn echoes into scrollback with the SAME `›`, so the LAST `›` is the
/// live composer).
const CODEX_PROMPT_GLYPH: char = '\u{203a}'; // ›

/// codex's clear-chord: Ctrl-U (0x15). No single full-clear binding exists by
/// default (`/keymap`: Kill-Whole-Line UNBOUND); repeated Ctrl-U converges (~2L-1
/// presses for L lines) and is a SAFE no-op on an empty composer. Ctrl-C
/// (quits-on-empty) and double-Esc (backtrack pager EDITS history) are
/// DISQUALIFIED — never emitted.
const CODEX_CLEAR_CHORD: &[u8] = b"\x15";
/// Bounded Ctrl-U presses (covers a multi-line codex draft; over-pressing is a
/// safe no-op once empty). Deeper multi-line convergence is a real-PTY M5 concern;
/// a non-converged clear degrades safely (`reverify_plain`), never a blind inject.
const CODEX_CLEAR_PRESSES: u32 = 12;
/// Inter-press spacing so the repeated Ctrl-U settles between presses.
const CODEX_CLEAR_SETTLE_MS: u64 = 40;

/// codex per-harness facts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexFacts;

impl HarnessFacts for CodexFacts {
    fn clear_chord(&self) -> Vec<u8> {
        CODEX_CLEAR_CHORD.to_vec()
    }

    /// QS-5 SAFETY-CRITICAL: the STATUS-LINE discriminator, NOT glyph-presence.
    /// The `/model` modal's selection marker is ALSO `›`, so glyph-presence would
    /// false-positive the modal as plain (a digit moves selection, Enter CONFIRMS
    /// a model change) — the concrete blind-typing hazard. So:
    /// - provably NOT plain (`Some(false)`) on the `/model` modal and the
    ///   double-Esc backtrack pager (Enter there EDITS a previous message);
    /// - plain (`Some(true)`) only when a status line `<model> <effort> · <cwd>`
    ///   (a ` · ` mid-dot line) is present AND the composer glyph is on screen;
    /// - UNKNOWN (`None`) otherwise — e.g. the transient `esc again to edit
    ///   previous message` hint that replaces the status line → honest
    ///   verify-blocked, never a blind type.
    fn composer_is_plain(&self, screen_text: &str) -> Option<bool> {
        // N1 (M4 red-team r1, defense-in-depth, deferred to M5): this denylist is
        // inherently non-exhaustive. It is NOT the primary guard — the predicate is
        // ALLOWLIST-shaped (a status line ` · ` is REQUIRED below), and modals
        // generally REPLACE the status line, so an un-enumerated modal already
        // degrades safe (→ None → verify-blocked). The denylist only hardens the
        // two dangerous screens that DO keep a status-line-like row. M5 real-drive
        // should enumerate codex's remaining confirm-dialog surfaces (e.g.
        // /approvals, permission prompts, /init).
        const MODAL_SIGS: &[&str] = &[
            "Select Model and Effort",                   // /model dialog header
            "Press enter to confirm or esc to go back",  // /model dialog footer
            "q to quit",                                 // backtrack/transcript pager
        ];
        // Shared modal discriminator (QS-5) — the SAME mechanism SafeDefaultFacts
        // now uses for claude's `/model` modal (REUSE, not a forked copy).
        if modal_screen_is_denied(screen_text, MODAL_SIGS) {
            return Some(false);
        }
        let has_status_line = screen_text.lines().any(|l| l.contains(" \u{00b7} ")); // " · "
        if has_status_line && screen_text.contains(CODEX_PROMPT_GLYPH) {
            return Some(true);
        }
        None
    }

    fn composer_region(&self) -> ComposerRegion {
        ComposerRegion::GlyphAnchor(CODEX_PROMPT_GLYPH)
    }

    /// codex publishes no pollable busy/idle status (the Q7 residual that gated
    /// its delivery off entirely), but every codex session writes its rollout, and
    /// a submitted message appears there as a `response_item` user record with the
    /// exact input text. That is the acceptance proof — verified against a live
    /// codex-cli 0.146.1 rollout, from which `CodexLandingProbe` extracts typed
    /// messages verbatim.
    fn acceptance_signal(&self) -> AcceptanceSignal {
        AcceptanceSignal::Landing
    }

    fn clear_strategy(&self) -> ClearStrategy {
        ClearStrategy {
            chord: CODEX_CLEAR_CHORD.to_vec(),
            presses: CODEX_CLEAR_PRESSES,
            settle_ms: CODEX_CLEAR_SETTLE_MS,
            reverify_plain: true,
        }
    }
}

// ===========================================================================
// PiFacts — pi 0.80.2 (Q7 note 01KXAPXNH0, verified at source).
// ===========================================================================

/// pi's composer has NO prompt glyph. Its full-width horizontal rule `─` U+2500
/// (UTF-8 `e2 94 80`) frames the composer: the region is BETWEEN THE LAST TWO
/// rules (footer below the bottom rule, scrollback above the top rule).
const PI_RULE: char = '\u{2500}'; // ─

/// pi's clear-chord: a SINGLE Ctrl-C (0x03) — one-press full clear, pi stays alive
/// on a held composer. HAZARD: a rapid/double Ctrl-C EXITS pi, so the strategy
/// presses EXACTLY ONCE (`presses == 1`) and never re-sends it within a clear;
/// across fires the inject/re-show bytes always interpose, so two Ctrl-C are never
/// consecutive. Ctrl-D (exits-on-empty) is never emitted.
const PI_CLEAR_CHORD: &[u8] = b"\x03";
/// A post-press settle (the pi clear-chord's single press; also a de-arming pause
/// against any rapid re-press).
const PI_CLEAR_SETTLE_MS: u64 = 60;

/// pi per-harness facts.
#[derive(Debug, Default, Clone, Copy)]
pub struct PiFacts;

impl HarnessFacts for PiFacts {
    fn clear_chord(&self) -> Vec<u8> {
        PI_CLEAR_CHORD.to_vec()
    }

    /// Plain iff two trailing full-width `─` rules frame the composer AND there is
    /// no `→` selection marker between them. The `/model` overlay (also `ctrl+l`)
    /// drops one rule and shows `→` markers + its own `>` search prompt — a blind
    /// type there mutates model state. `Some(false)` on that overlay; `Some(true)`
    /// on the framed plain composer; `None` if the frame is unlocatable (safe
    /// verify-blocked).
    fn composer_is_plain(&self, screen_text: &str) -> Option<bool> {
        // The model overlay: a `→` selection marker present ⇒ provably NOT plain.
        if screen_text.contains('\u{2192}') {
            return Some(false);
        }
        // Plain iff at least two full-width `─` rule lines are present (the
        // composer frame). Fewer ⇒ unknown (verify-blocked), never a blind type.
        let rule_lines = screen_text
            .lines()
            .filter(|l| {
                let mut n = 0usize;
                for ch in l.chars() {
                    if ch == PI_RULE {
                        n += 1;
                    } else if !ch.is_whitespace() {
                        return false;
                    }
                }
                n >= 3
            })
            .count();
        if rule_lines >= 2 {
            Some(true)
        } else {
            None
        }
    }

    fn composer_region(&self) -> ComposerRegion {
        ComposerRegion::BetweenLastTwo(PI_RULE)
    }

    /// pi publishes no pollable busy/idle status either, but — like codex — it
    /// RECORDS THE SUBMITTED MESSAGE, so the landing is the acceptance proof.
    ///
    /// THIS CORRECTS A RECORDED FACT. The M5 observation held that pi's transcript
    /// was "append-on-exit", which would make a live landing unobservable and is
    /// why pi stayed gated when codex un-gated. Read at source (pi 0.80.2,
    /// `dist/core/session-manager.js` `_persist`), it is not: persist runs on
    /// EVERY appended entry and defers only until the buffer holds an assistant
    /// message — after which each entry, including a user message, is
    /// `appendFileSync`'d immediately. A session reopened from an existing file
    /// (`setSessionFile` → `flushed = true`) appends from its very first entry.
    ///
    /// pi's user records are `{"type":"message","message":{"role":"user",…}}`,
    /// which [`TranscriptLandingProbe`] already parses — the probe was broadened
    /// for exactly this shape — so pi needs no probe of its own.
    ///
    /// THE RESIDUAL, stated honestly rather than papered over: a FRESH session
    /// that has not yet had an assistant reply has no file on disk at all, so a
    /// landing cannot be confirmed and the send reports non-delivery. That is the
    /// truthful answer for that window, and it closes itself after one exchange.
    fn acceptance_signal(&self) -> AcceptanceSignal {
        AcceptanceSignal::Landing
    }

    fn clear_strategy(&self) -> ClearStrategy {
        ClearStrategy {
            chord: PI_CLEAR_CHORD.to_vec(),
            presses: 1, // SINGLE press — a rapid second Ctrl-C EXITS pi.
            settle_ms: PI_CLEAR_SETTLE_MS,
            reverify_plain: true,
        }
    }
}

// ===========================================================================
// LandingProbe — the bounded post-fire transcript verify (RT-R2 message-seen).
// ===========================================================================

/// One scan of the transcript window for the sent payload's landing.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LandingScan {
    /// A user record at/after the offset matches the sent message byte-exact.
    Landed,
    /// A truncated candidate (shared prefix, shorter) landed — carries the actual
    /// record's sha/len for an honest `turn-anchored-mismatch`.
    Mismatch { actual_sha: String, actual_len: u64 },
    /// No landing confirmable this scan (not yet landed / unreadable / no match).
    Unconfirmed,
}

impl LandingScan {
    /// Collapse to the pure-core [`LandingResult`] (the reconcile path's alphabet).
    pub fn to_result(&self) -> LandingResult {
        match self {
            LandingScan::Landed => LandingResult::Landed,
            LandingScan::Mismatch { .. } => LandingResult::Mismatch,
            LandingScan::Unconfirmed => LandingResult::Unconfirmed,
        }
    }
}

/// The landing verify seam (a seam so the transcript-verify can be reused/extracted
/// rather than forked). M1 ships [`TranscriptLandingProbe`]; tests inject a fake.
pub trait LandingProbe: Send + Sync {
    /// One scan of `transcript` past `offset` for `message`'s landing (the live
    /// fire path — it has the sent text).
    fn scan(&self, transcript: Option<&str>, offset: Option<u64>, message: &str) -> LandingScan;

    /// One scan keyed on the content SHA (the RECONCILE path — a restart has the
    /// durable `content_sha256`/`content_len`, not the sent text). Default:
    /// `Unconfirmed` (a fake with no transcript can never confirm). Exact sha match
    /// ⇒ `Landed`; else `Unconfirmed` (M1 reconcile does not detect truncation —
    /// it lacks the message bytes; that refinement is M4/M5). `content_len` is
    /// carried for that future truncation work.
    fn scan_sha(
        &self,
        _transcript: Option<&str>,
        _offset: Option<u64>,
        _content_sha256: &str,
        _content_len: u64,
    ) -> LandingScan {
        LandingScan::Unconfirmed
    }
}

/// M1's shipping landing probe: read the JSONL transcript from `offset`, extract
/// the user-record texts, and match `message` byte-exact (a truncated shared-prefix
/// candidate → `Mismatch`). Mirrors dispatch's `verify_chunked_payload` matching
/// shape; the transcript record model (`type:"user"`, `message.content`) is
/// harness-generic here and M4-refinable behind the seam.
#[derive(Debug, Default, Clone, Copy)]
pub struct TranscriptLandingProbe;

impl LandingProbe for TranscriptLandingProbe {
    fn scan(&self, transcript: Option<&str>, offset: Option<u64>, message: &str) -> LandingScan {
        let path = match transcript {
            Some(p) => p,
            None => return LandingScan::Unconfirmed, // no transcript key → can't confirm
        };
        let texts = match user_texts_from(path, offset.unwrap_or(0)) {
            Ok(t) => t,
            Err(_) => return LandingScan::Unconfirmed, // unreadable → can't confirm
        };
        classify_landing(&texts, message)
    }

    fn scan_sha(
        &self,
        transcript: Option<&str>,
        offset: Option<u64>,
        content_sha256: &str,
        _content_len: u64,
    ) -> LandingScan {
        let path = match transcript {
            Some(p) => p,
            None => return LandingScan::Unconfirmed,
        };
        let texts = match user_texts_from(path, offset.unwrap_or(0)) {
            Ok(t) => t,
            Err(_) => return LandingScan::Unconfirmed,
        };
        // Exact sha match ⇒ Landed. (Reconcile lacks the message bytes, so no
        // prefix/truncation check here — Unconfirmed otherwise, never a false
        // landed.)
        if texts
            .iter()
            .any(|t| quorum_delivery_events::sha256_hex(t.as_bytes()) == content_sha256)
        {
            LandingScan::Landed
        } else {
            LandingScan::Unconfirmed
        }
    }
}

/// The codex landing probe (M4): codex rollouts (`~/.codex/sessions/Y/M/D/
/// rollout-<ts>-<uuid>.jsonl`) use a `response_item`/`payload.message` shape the
/// default [`TranscriptLandingProbe`] can never match. This probe parses that
/// shape via [`codex_user_texts_from`], then classifies identically (byte-exact
/// landed / truncation mismatch / unconfirmed). This is what M5's real-drive codex
/// delivery proof consumes to un-defer codex delivery.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexLandingProbe;

impl LandingProbe for CodexLandingProbe {
    fn scan(&self, transcript: Option<&str>, offset: Option<u64>, message: &str) -> LandingScan {
        let path = match transcript {
            Some(p) => p,
            None => return LandingScan::Unconfirmed,
        };
        let texts = match codex_user_texts_from(path, offset.unwrap_or(0)) {
            Ok(t) => t,
            Err(_) => return LandingScan::Unconfirmed,
        };
        classify_landing(&texts, message)
    }

    fn scan_sha(
        &self,
        transcript: Option<&str>,
        offset: Option<u64>,
        content_sha256: &str,
        _content_len: u64,
    ) -> LandingScan {
        let path = match transcript {
            Some(p) => p,
            None => return LandingScan::Unconfirmed,
        };
        let texts = match codex_user_texts_from(path, offset.unwrap_or(0)) {
            Ok(t) => t,
            Err(_) => return LandingScan::Unconfirmed,
        };
        if texts
            .iter()
            .any(|t| quorum_delivery_events::sha256_hex(t.as_bytes()) == content_sha256)
        {
            LandingScan::Landed
        } else {
            LandingScan::Unconfirmed
        }
    }
}

/// Classify the landing of `message` against the user-record `texts` past the
/// floor: an exact match is [`LandingScan::Landed`]; else the LONGEST text that is
/// a strict prefix of `message` (a truncation) is [`LandingScan::Mismatch`]; else
/// [`LandingScan::Unconfirmed`]. Pure — no fs/clock.
pub fn classify_landing(texts: &[String], message: &str) -> LandingScan {
    if texts.iter().any(|t| t == message) {
        return LandingScan::Landed;
    }
    // A truncation signature: a strictly-shorter text that is a prefix of the sent
    // message (the same "shared-prefix, shorter" shape dispatch keys on).
    let truncated = texts
        .iter()
        .filter(|t| t.len() < message.len() && message.starts_with(t.as_str()) && !t.is_empty())
        .max_by_key(|t| t.len());
    match truncated {
        Some(t) => LandingScan::Mismatch {
            actual_sha: quorum_delivery_events::sha256_hex(t.as_bytes()),
            actual_len: t.len() as u64,
        },
        None => LandingScan::Unconfirmed,
    }
}

/// Extract user-record message texts from a JSONL transcript past `offset`.
/// Accepts BOTH shapes whose `message.content` is claude-shaped:
/// - claude: a top-level `{"type":"user", "message":{...}}` record;
/// - pi 0.80.2: a top-level `{"type":"message","message":{"role":"user",...}}`
///   record (pi's `message.content` is already `[{"type":"text","text":…}]`, so
///   [`extract_message_text`] parses it as-is — the one-line-away default match).
/// Each matched record's `message.content` (a string, or an array of
/// `{"type":"text","text":...}` parts concatenated) yields one text. Best-effort:
/// malformed lines are skipped. (codex rollouts are a DIFFERENT shape — see
/// [`CodexLandingProbe`].)
fn user_texts_from(path: &str, offset: u64) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    if offset > 0 {
        // Clamp the floor to the file length so a stale/oversized offset reads
        // nothing rather than erroring.
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(offset.min(len)))?;
    }
    let mut body = String::new();
    f.read_to_string(&mut body)?;
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|t| t.as_str());
        // claude: type=="user". pi: type=="message" && message.role=="user".
        let is_user_record = ty == Some("user")
            || (ty == Some("message")
                && v.get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str())
                    == Some("user"));
        if !is_user_record {
            continue;
        }
        if let Some(text) = extract_message_text(v.get("message")) {
            out.push(text);
        }
    }
    Ok(out)
}

/// Extract codex user-record texts from a codex rollout JSONL past `offset`. A
/// codex user turn is `{"type":"response_item","payload":{"type":"message",
/// "role":"user","content":[{"type":"input_text","text":"…"}],…}}` (verified at
/// source, codex 0.144.1) — a shape the claude/pi [`user_texts_from`] can NEVER
/// match (nested under `payload`; `input_text` not `text`). Best-effort; malformed
/// lines skipped.
fn codex_user_texts_from(path: &str, offset: u64) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    if offset > 0 {
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(offset.min(len)))?;
    }
    let mut body = String::new();
    f.read_to_string(&mut body)?;
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("message")
            || payload.get("role").and_then(|r| r.as_str()) != Some("user")
        {
            continue;
        }
        if let Some(text) = extract_input_text(payload.get("content")) {
            out.push(text);
        }
    }
    Ok(out)
}

/// codex `payload.content` → text: the concatenation of the `text` fields of the
/// `input_text` parts (ignoring non-text parts). `None` if empty.
fn extract_input_text(content: Option<&serde_json::Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let mut buf = String::new();
    for part in arr {
        if part.get("type").and_then(|t| t.as_str()) == Some("input_text") {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                buf.push_str(t);
            }
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// `message.content` → text: a bare string, or the concatenation of the `text`
/// parts of a content array (ignoring tool_result / image parts).
fn extract_message_text(message: Option<&serde_json::Value>) -> Option<String> {
    let content = message?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut buf = String::new();
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    buf.push_str(t);
                }
            }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

// ===========================================================================
// The fire sequence.
// ===========================================================================

/// Tunable fire knobs (changeable without replan). Verify + landing windows.
#[derive(Debug, Clone, Copy)]
pub struct FireConfig {
    /// Attempts to observe a plain composer before honest `verify-blocked`.
    pub verify_attempts: u32,
    /// Sleep between plain-composer verify attempts (ms).
    pub verify_retry_ms: u64,
    /// Bounded post-fire landing-verify window (ms) before the send stays Pending.
    pub landing_window_ms: i64,
    /// Landing-verify poll interval (ms).
    pub landing_poll_ms: u64,
    /// The submit discipline's acceptance windows.
    pub submit: SubmitOptions,
}

impl Default for FireConfig {
    fn default() -> Self {
        Self {
            verify_attempts: 3,
            verify_retry_ms: 200,
            landing_window_ms: 8_000,
            landing_poll_ms: 250,
            submit: SubmitOptions::default(),
        }
    }
}

/// The outcome of a live fire. (`Payload` is `PartialEq` but not `Eq`, so this is
/// `PartialEq` only — sufficient for the deterministic gates.)
#[derive(Debug, PartialEq, Clone)]
pub enum FireOutcome {
    /// Emit this terminal payload to the ledger, notify `--wait`, and REMOVE the
    /// spool record.
    Terminal(Payload),
    /// Injected + accepted, but the landing is not yet confirmable within the
    /// bounded window. NO terminal: the record stays spooled at `FireCompleted`;
    /// `--wait`/reconcile resolves it. Never a false "landed".
    Pending,
}

/// Drive one pending send onto the PTY. Blocking — run in `spawn_blocking`.
///
/// `rec` is the send's durable spool record (already write-ahead-spooled at
/// acceptance). `message` is the exact text to submit; `draft` is the human's
/// in-progress journal draft (replayed after the inject, P4). `lock`/`journal` are
/// the session's shared state (std mutexes, taken briefly). The fire updates `rec`
/// at the durable write points and returns the outcome; on `Terminal` the caller
/// emits `rec`-keyed vocab payload + removes the spool record.
#[allow(clippy::too_many_arguments)]
pub fn fire(
    effects: &dyn FireEffects,
    facts: &dyn HarnessFacts,
    probe: &dyn LandingProbe,
    lock: &Mutex<InputLock>,
    journal: &Mutex<Journal>,
    spool: &Spool,
    mut rec: PendingRecord,
    message: &str,
    cfg: &FireConfig,
) -> FireOutcome {
    // 0. ACCEPTANCE-CONFIRMABLE GATE (M4 F1) — the fire-eligibility floor for a
    //    harness whose acceptance is UNCONFIRMABLE (no landed busy/idle status
    //    source; codex/pi Q7 residual). We must NOT clear/inject here: injecting a
    //    real turn we can never observe accepted would report a real delivery as a
    //    failure (F1's false-negative + double-submit hazard), and
    //    `composer_is_plain → Some(true)` alone cannot rule out a mid-turn (busy)
    //    composer. So resolve to an HONEST non-delivery terminal with the composer
    //    UNTOUCHED — no lock, no snapshot, no clear, no inject, no CR, nothing to
    //    re-show, no delivery lie, no double-submit. This is the STATUS-fact analog
    //    of the QS-5 composer-fact gate (a missing status fact degrades to no-fire,
    //    exactly as a missing composer fact degrades to verify-blocked). claude
    //    (`claude_default`) is confirmable and skips this gate → fires normally,
    //    byte-for-byte. When the codex/pi status source lands (M5), this un-gates.
    //
    // M5/T6 UN-GATE: the gate asks whether acceptance is confirmable AT ALL, and a
    // missing busy/idle status is no longer the same question. A
    // `AcceptanceSignal::Landing` harness (codex, and now pi) confirms acceptance
    // from its TRANSCRIPT instead — strictly stronger evidence than a busy
    // transition, since it identifies the exact bytes. So the gate closes only on
    // a harness with NEITHER signal.
    //
    // pi-interactive: with pi moved to Landing, NO HARNESS SHIPPED TODAY CLOSES
    // THIS GATE — claude has a status source, codex and pi have landings. That is
    // worth stating plainly rather than leaving the reader to infer it from three
    // files. The gate stays because it is the FLOOR, not because it currently
    // fires: it is what a future carrier with neither signal falls to, and the
    // alternative to keeping it is injecting turns we can never observe accepted.
    // `gate_closes_on_a_harness_with_neither_signal` covers it deliberately.
    if !effects.acceptance_confirmable() && facts.acceptance_signal() != AcceptanceSignal::Landing {
        // M5 observability nicety (M4-noted): a DISTINCT reason from the QS-5
        // plain-composer `verify-blocked` below. Both are honest non-deliveries with
        // the composer untouched, but this one means "the harness has no confirmable
        // acceptance signal", NOT "a modal/unknown composer blocked the type" —
        // so a stranger reading the ledger can tell a carrier blocked on
        // acceptance-confirmability from one blocked by a composer modal.
        // Still `send-failed` (the leaf KIND is unchanged; only the free reason
        // detail differs — no minted kind string).
        return FireOutcome::Terminal(Payload::SendFailed {
            send_id: Some(rec.send_id.clone()),
            content_sha256: rec.content_sha256.clone(),
            reason: "acceptance-unconfirmable".to_string(),
        });
    }

    // 1. LOCK + SNAPSHOT (atomic) — arm the input lock and snapshot the draft while
    //    STILL holding the input lock, so this (arm, snapshot) pair is atomic w.r.t.
    //    the input path's (journal-append, admit, passthrough-write) under the SAME
    //    input lock (driver.rs `journal_admit_passthrough`). This is what closes the
    //    QS-1 duplication race: a keystroke racing fire-start lands in the snapshot
    //    (admitted Passthrough before the arm → its pre-clear passthrough byte is
    //    wiped by the clear, then replayed once, P4) XOR the buffer (admitted
    //    Buffered after the arm → flushed once on unlock), never BOTH and never neither.
    //    Byte-exact (QS-2); the snapshot is also the P4 preserved copy in the record.
    let draft = lock_and_snapshot(lock, journal);

    // 2. PLAIN-COMPOSER VERIFY (QS-5) — GATES the clear/inject. Bounded retry;
    //    not-plain / unknown ⇒ honest verify-blocked, NEVER a blind type.
    if !verify_plain_composer(effects, facts, cfg) {
        return finish_failure(
            effects,
            lock,
            &draft,
            /*cleared=*/ false,
            rec,
            "verify-blocked",
        );
    }

    // 3. FIRE-START durable write BEFORE the clear-chord (RT-R1): a crash after
    //    this reconciles as "inject MAY have run" (never re-injects).
    rec.phase = FirePhase::FireStarted;
    rec.fire_started = true;
    rec.draft = draft.clone();
    if spool.write(&rec).is_err() {
        // Cannot durably record fire-start ⇒ do NOT clear/inject (a crash would
        // misreconcile). Honest failure, composer untouched.
        return finish_failure(effects, lock, &draft, false, rec, "inject-failed");
    }

    // 4. CLEAR — empty the composer per the harness clear STRATEGY (M4): a single
    //    unconditional press (claude/pi) or a bounded, spaced, converge-by-repeat
    //    (codex). A write error is inject-failed; a composer that does not verify
    //    PLAIN after the bounded clear is an honest verify-blocked (QS-5 safe
    //    floor) — NEVER a blind inject. Both disturb the composer, so re-show.
    match clear_composer(effects, facts) {
        ClearOutcome::Cleared => {}
        ClearOutcome::WriteFailed => {
            return finish_failure(effects, lock, &draft, /*cleared=*/ true, rec, "inject-failed");
        }
        ClearOutcome::NotPlain => {
            return finish_failure(effects, lock, &draft, /*cleared=*/ true, rec, "verify-blocked");
        }
    }

    // 5. INJECT — the shared discipline: chunked two-write text + content-verified
    //    verify-then-CR (submits ONLY while the composer holds our text — never
    //    blind, QS-5). The content-verify anchors on the PER-HARNESS composer
    //    region (claude/codex glyph, pi two-rule). The remediation CR is bounded to
    //    ≤1 by the core.
    match facts.acceptance_signal() {
        AcceptanceSignal::BusyTransition => {
            let outcome = deliver_idle_two_write_in_region(
                &AsIdle(effects),
                message,
                cfg.submit,
                facts.composer_region(),
            );
            if !outcome.accepted {
                // The session never went busy: the turn was not accepted. Honest failure.
                return finish_failure(effects, lock, &draft, true, rec, "not-accepted");
            }
        }
        AcceptanceSignal::Landing => {
            // The SAME two-write shape (chunked text, settle, separate CR — the
            // shape a paste burst does not collapse), but WITHOUT the
            // acceptance-keyed remediation loop.
            //
            // The remediation CR is keyed on "the status says we did not go busy".
            // With no status signal that key is always true, so the loop would fire
            // an unconditional second CR on every send — the double-submit the F1
            // gate existed to prevent. Dropping it costs the one-CR recovery for an
            // absorbed CR; that case now surfaces as an honest non-delivery the
            // caller retries, and a retry is SAFE because step 4 clears the composer
            // first (the stale text cannot compound).
            //
            // Acceptance is then decided by the landing verify below, which is the
            // point of this signal: we do not guess from the screen, we read what
            // the harness actually recorded.
            send_text_chunked(
                &mut |chunk| effects.send_text(chunk),
                &mut |ms| effects.sleep(ms),
                message,
                ChunkSendOptions::default(),
            );
            effects.sleep(TWO_WRITE_SETTLE_MS);
            effects.send_cr();
        }
    }

    // Accepted ⇒ mark fire_completed durable (a crash now reconciles by probing
    // the transcript; a failed write here is tolerable — still never re-injects).
    rec.phase = FirePhase::FireCompleted;
    rec.fire_completed = true;
    let _ = spool.write(&rec);

    // 6. REPLAY the human's draft into the now-submitted composer (P4), then
    // 7. UNLOCK + flush the buffered keystrokes in order (QS-1). Replayed/flushed
    //    bytes are raw PTY writes; they do NOT enter the journal.
    if !draft.is_empty() {
        let _ = effects.write_raw(&draft);
    }
    let buffered = unlock_input(lock);
    if !buffered.is_empty() {
        let _ = effects.write_raw(&buffered);
    }

    // 8. BOUNDED LANDING VERIFY (RT-R2) — a success terminal requires a CONFIRMED
    //    landing; mere acceptance is not terminal (no false "landed").
    landing_terminal(effects, probe, &rec, message, cfg, facts.acceptance_signal())
}

/// Arm the input lock and snapshot the draft ATOMICALLY: acquire the input lock,
/// arm it, then snapshot the journal while STILL holding the input lock — so this
/// (arm, snapshot) pair cannot interleave with the input path's (journal-append,
/// admit, passthrough-write) under the SAME input lock (driver.rs
/// `journal_admit_passthrough`) — the passthrough write of a racing keystroke is
/// therefore pre-clear (it lands before this arm can take the lock). This closes the
/// QS-1 keystroke-duplication race. Consistent lock order everywhere both are held:
/// INPUT LOCK before JOURNAL. Poisoned locks are recovered (the flag / draft is the
/// only state).
fn lock_and_snapshot(lock: &Mutex<InputLock>, journal: &Mutex<Journal>) -> Vec<u8> {
    let mut l = lock.lock().unwrap_or_else(|p| p.into_inner());
    l.lock();
    match journal.lock() {
        Ok(j) => j.snapshot(),
        Err(p) => p.into_inner().snapshot(),
    }
}

/// Release the input lock and drain the buffered bytes in order.
fn unlock_input(lock: &Mutex<InputLock>) -> Vec<u8> {
    match lock.lock() {
        Ok(mut l) => l.unlock_and_drain(),
        Err(p) => p.into_inner().unlock_and_drain(),
    }
}

/// Bounded plain-composer verify (QS-5): `Some(true)` proceeds; anything else,
/// after `verify_attempts` reads, is a block.
fn verify_plain_composer(
    effects: &dyn FireEffects,
    facts: &dyn HarnessFacts,
    cfg: &FireConfig,
) -> bool {
    for attempt in 0..cfg.verify_attempts.max(1) {
        if facts.composer_is_plain(&effects.read_screen()) == Some(true) {
            return true;
        }
        if attempt + 1 < cfg.verify_attempts.max(1) {
            effects.sleep(cfg.verify_retry_ms);
        }
    }
    false
}

/// The result of the clear step.
enum ClearOutcome {
    /// The composer was cleared (per the strategy) and, if required, re-verified
    /// plain — safe to inject.
    Cleared,
    /// A clear-chord write failed → honest `inject-failed`.
    WriteFailed,
    /// The bounded clear left a composer that does not verify PLAIN (a modal, or a
    /// non-converged clear) → honest `verify-blocked`, never a blind inject.
    NotPlain,
}

/// Drive the harness [`ClearStrategy`] (M4): press the chord `presses` times,
/// sleeping `settle_ms` after each (spacing a repeated chord and rate-limiting so
/// a chord is never re-sent faster than the harness tolerates — pi's single-press
/// Ctrl-C never chains a rapid second). If `reverify_plain`, re-confirm the
/// composer is plain after the presses, else degrade (QS-5 safe floor).
fn clear_composer(effects: &dyn FireEffects, facts: &dyn HarnessFacts) -> ClearOutcome {
    let strat = facts.clear_strategy();
    for _ in 0..strat.presses.max(1) {
        if effects.write_raw(&strat.chord).is_err() {
            return ClearOutcome::WriteFailed;
        }
        if strat.settle_ms > 0 {
            effects.sleep(strat.settle_ms);
        }
    }
    if strat.reverify_plain && facts.composer_is_plain(&effects.read_screen()) != Some(true) {
        return ClearOutcome::NotPlain;
    }
    ClearOutcome::Cleared
}

/// Common failure tail: unlock + flush, re-show the preserved draft if we had
/// cleared the composer (P4/QS-4), and return the honest `send-failed{reason}`.
fn finish_failure(
    effects: &dyn FireEffects,
    lock: &Mutex<InputLock>,
    draft: &[u8],
    cleared: bool,
    rec: PendingRecord,
    reason: &str,
) -> FireOutcome {
    // Re-show the draft only if we disturbed the composer (post-clear); if we never
    // cleared, the human's words are still on screen untouched.
    if cleared && !draft.is_empty() {
        let _ = effects.write_raw(draft);
    }
    let buffered = unlock_input(lock);
    if !buffered.is_empty() {
        let _ = effects.write_raw(&buffered);
    }
    FireOutcome::Terminal(Payload::SendFailed {
        send_id: Some(rec.send_id.clone()),
        content_sha256: rec.content_sha256.clone(),
        reason: reason.to_string(),
    })
}

/// The bounded landing verify → terminal. Polls the probe until the payload is
/// confirmed landed/mismatched, the hosted session is observed gone
/// (recipient-gone), or the window elapses (Pending).
fn landing_terminal(
    effects: &dyn FireEffects,
    probe: &dyn LandingProbe,
    rec: &PendingRecord,
    message: &str,
    cfg: &FireConfig,
    signal: AcceptanceSignal,
) -> FireOutcome {
    let deadline = effects.now_ms() + cfg.landing_window_ms;
    loop {
        match probe.scan(rec.transcript.as_deref(), rec.transcript_offset, message) {
            LandingScan::Landed => {
                return FireOutcome::Terminal(Payload::MessageSeen {
                    send_id: rec.send_id.clone(),
                    content_sha256: rec.content_sha256.clone(),
                });
            }
            LandingScan::Mismatch {
                actual_sha,
                actual_len,
            } => {
                return FireOutcome::Terminal(Payload::TurnAnchoredMismatch {
                    send_id: rec.send_id.clone(),
                    expected_sha: rec.content_sha256.clone(),
                    actual_sha,
                    expected_len: rec.content_len,
                    actual_len,
                    recovered: false,
                    attribution: None,
                });
            }
            LandingScan::Unconfirmed => {
                // Session observed gone before landing confirmed ⇒ recipient-gone
                // (QS-4). `None` status = the status source vanished (session died).
                //
                // M5/T6: that inference is only sound for a harness that HAS a
                // status source. A Landing harness reads `None` always and by
                // design, so applying it here would report `recipient-gone` on the
                // very first poll of a perfectly healthy codex session that simply
                // has not flushed its rollout line yet — turning the normal case
                // into a false death. For Landing, absence of a landing is just
                // "not yet": poll to the deadline and stay Pending.
                if signal == AcceptanceSignal::BusyTransition && effects.read_status().is_none() {
                    return FireOutcome::Terminal(Payload::SeenFailed {
                        send_id: rec.send_id.clone(),
                        reason: "recipient-gone".to_string(),
                    });
                }
                if effects.now_ms() >= deadline {
                    // Not confirmable within the window: stay Pending, no terminal
                    // (never a false landed). --wait / reconcile resolves it.
                    return FireOutcome::Pending;
                }
                effects.sleep(cfg.landing_poll_ms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    // ---- A scriptable FireEffects fake -------------------------------------

    /// A scripted effects fake: a virtual clock, a queue of screen renders (last
    /// repeats), a queue of statuses (last repeats), a record of every raw/text/CR
    /// write, and an optional clear-write failure.
    struct FakeFx {
        clock: AtomicI64,
        screens: Mutex<std::collections::VecDeque<String>>,
        statuses: Mutex<std::collections::VecDeque<Option<String>>>,
        writes: Mutex<Vec<Vec<u8>>>,
        texts: Mutex<Vec<String>>,
        crs: AtomicI64,
        fail_raw: Mutex<bool>,
        /// M4 F1: whether this harness's acceptance is confirmable. Defaults TRUE
        /// (claude-shaped — the fire runs), so every existing test is unchanged;
        /// the F1 test sets it false to prove the no-inject gate.
        confirmable: Mutex<bool>,
    }
    impl FakeFx {
        fn new(screens: Vec<&str>, statuses: Vec<Option<&str>>) -> Self {
            Self {
                clock: AtomicI64::new(0),
                screens: Mutex::new(screens.into_iter().map(String::from).collect()),
                statuses: Mutex::new(
                    statuses
                        .into_iter()
                        .map(|s| s.map(String::from))
                        .collect(),
                ),
                writes: Mutex::new(Vec::new()),
                texts: Mutex::new(Vec::new()),
                crs: AtomicI64::new(0),
                fail_raw: Mutex::new(false),
                confirmable: Mutex::new(true),
            }
        }
        fn pop_repeat<T: Clone>(q: &Mutex<std::collections::VecDeque<T>>, default: T) -> T {
            let mut q = q.lock().unwrap();
            if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap_or(default)
            }
        }
        fn raw_writes(&self) -> Vec<Vec<u8>> {
            self.writes.lock().unwrap().clone()
        }
    }
    impl FireEffects for FakeFx {
        fn send_text(&self, text: &str) {
            self.texts.lock().unwrap().push(text.to_string());
        }
        fn send_cr(&self) {
            self.crs.fetch_add(1, Ordering::SeqCst);
        }
        fn write_raw(&self, bytes: &[u8]) -> std::io::Result<()> {
            if *self.fail_raw.lock().unwrap() {
                return Err(std::io::Error::other("forced clear-chord write failure"));
            }
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn read_screen(&self) -> String {
            Self::pop_repeat(&self.screens, String::new())
        }
        fn read_status(&self) -> Option<String> {
            Self::pop_repeat(&self.statuses, None)
        }
        fn acceptance_confirmable(&self) -> bool {
            *self.confirmable.lock().unwrap()
        }
        fn sleep(&self, ms: u64) {
            self.clock.fetch_add(ms as i64, Ordering::SeqCst);
        }
        fn now_ms(&self) -> i64 {
            self.clock.load(Ordering::SeqCst)
        }
    }

    struct FixedProbe(LandingScan);
    impl LandingProbe for FixedProbe {
        fn scan(&self, _t: Option<&str>, _o: Option<u64>, _m: &str) -> LandingScan {
            self.0.clone()
        }
    }

    fn rec() -> PendingRecord {
        PendingRecord::accepted(
            "p-1",
            quorum_delivery_events::sha256_hex(b"hello"),
            5,
            Some("sid".into()),
            Some("alpha".into()),
            "send:pty",
            false,
            0,
        )
    }

    fn cfg_fast() -> FireConfig {
        FireConfig {
            verify_attempts: 3,
            verify_retry_ms: 10,
            landing_window_ms: 1_000,
            landing_poll_ms: 50,
            submit: SubmitOptions {
                settle_ms: 10,
                post_cr_ms: 20,
                poll_ms: 5,
            },
        }
    }

    fn scratch_spool() -> (tempfile::TempDir, Spool) {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path().join("pending")).unwrap();
        (dir, spool)
    }

    // ---- QS-5: verify GATES clear/inject; blocked ⇒ honest failure, no blind type

    #[test]
    fn verify_blocked_never_clears_or_injects_and_fails_honestly() {
        // A composer with NO prompt glyph → SafeDefaultFacts returns None (unknown)
        // → verify-blocked. Nothing is written to the PTY (no clear, no text, no CR).
        let fx = FakeFx::new(vec!["a modal palette, no glyph"], vec![Some("idle")]);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        match out {
            FireOutcome::Terminal(Payload::SendFailed { reason, .. }) => {
                assert_eq!(reason, "verify-blocked");
            }
            other => panic!("expected send-failed verify-blocked, got {other:?}"),
        }
        assert!(fx.raw_writes().is_empty(), "no clear-chord write on a blocked verify");
        assert!(fx.texts.lock().unwrap().is_empty(), "no inject on a blocked verify");
        assert_eq!(fx.crs.load(Ordering::SeqCst), 0, "no CR on a blocked verify");
        // The lock was released (not left wedged).
        assert!(!lock.lock().unwrap().is_locked());
    }

    // ---- Happy path: plain composer → clear → inject → landed → message-seen

    #[test]
    fn plain_composer_fires_clears_injects_and_lands_message_seen() {
        // Screen shows the glyph AND holds our message after inject (so the
        // content-verified CR is satisfied). Status goes busy → accepted.
        let screen = format!("{PROMPT_GLYPH} hello");
        let fx = FakeFx::new(vec![screen.as_str()], vec![Some("busy")]);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        assert_eq!(
            out,
            FireOutcome::Terminal(Payload::MessageSeen {
                send_id: "p-1".into(),
                content_sha256: quorum_delivery_events::sha256_hex(b"hello"),
            })
        );
        // The clear-chord (Ctrl-U) was written before the inject.
        assert_eq!(fx.raw_writes()[0], b"\x15".to_vec(), "clear-chord fired first");
        // The text was injected (chunked send_text).
        assert!(!fx.texts.lock().unwrap().is_empty(), "text injected");
        assert!(!lock.lock().unwrap().is_locked(), "lock released after fire");
    }

    // ---- Accepted but landing unconfirmed within the window ⇒ Pending (no false landed)

    #[test]
    fn accepted_but_landing_unconfirmed_stays_pending_never_false_landed() {
        let screen = format!("{PROMPT_GLYPH} hello");
        let fx = FakeFx::new(vec![screen.as_str()], vec![Some("busy")]);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Unconfirmed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        assert_eq!(out, FireOutcome::Pending, "unconfirmed landing is never a terminal");
    }

    // ---- Accepted, session dies before landing ⇒ seen-failed{recipient-gone} (QS-4)

    #[test]
    fn session_gone_before_landing_is_seen_failed_recipient_gone() {
        let screen = format!("{PROMPT_GLYPH} hello");
        // Busy during inject (accepted), then status source vanishes (None) during
        // the landing poll.
        let fx = FakeFx::new(vec![screen.as_str()], vec![Some("busy"), None]);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Unconfirmed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        match out {
            FireOutcome::Terminal(Payload::SeenFailed { reason, .. }) => {
                assert_eq!(reason, "recipient-gone")
            }
            other => panic!("expected seen-failed recipient-gone, got {other:?}"),
        }
    }

    // ---- clear-chord write failure ⇒ send-failed{inject-failed} + re-show draft

    #[test]
    fn clear_chord_write_failure_is_inject_failed_and_reshows_draft() {
        let screen = format!("{PROMPT_GLYPH} ready");
        let fx = FakeFx::new(vec![screen.as_str()], vec![Some("idle")]);
        *fx.fail_raw.lock().unwrap() = true; // clear-chord write will fail
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let mut j = Journal::new();
        j.on_human_input(b"my draft", 0, 8);
        *journal.lock().unwrap() = j;
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        match out {
            FireOutcome::Terminal(Payload::SendFailed { reason, .. }) => {
                assert_eq!(reason, "inject-failed")
            }
            other => panic!("expected send-failed inject-failed, got {other:?}"),
        }
        assert!(!lock.lock().unwrap().is_locked(), "lock released on failure");
    }

    // ---- fire-start is durable BEFORE the clear-chord (RT-R1)

    #[test]
    fn fire_start_is_durable_before_clear_chord() {
        let screen = format!("{PROMPT_GLYPH} hello");
        let fx = FakeFx::new(vec![screen.as_str()], vec![Some("busy")]);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let r = rec();
        let sid = r.send_id.clone();
        let _ = spool.write(&r); // acceptance write-ahead
        let _ = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            r,
            "hello",
            &cfg_fast(),
        );
        // After a successful fire the record is FireCompleted + fire_started durable.
        let got = spool.load(&sid).unwrap().unwrap();
        assert!(got.fire_started, "fire_started persisted");
        assert!(got.fire_completed, "fire_completed persisted on accept");
    }

    // ---- classify_landing: exact vs truncation vs none ----------------------

    #[test]
    fn classify_landing_exact_truncation_and_none() {
        assert_eq!(classify_landing(&["hello".into()], "hello"), LandingScan::Landed);
        // A shorter shared-prefix candidate → mismatch with the actual sha/len.
        match classify_landing(&["hel".into()], "hello") {
            LandingScan::Mismatch { actual_len, .. } => assert_eq!(actual_len, 3),
            other => panic!("expected mismatch, got {other:?}"),
        }
        assert_eq!(
            classify_landing(&["unrelated".into()], "hello"),
            LandingScan::Unconfirmed
        );
        assert_eq!(classify_landing(&[], "hello"), LandingScan::Unconfirmed);
    }

    // ---- TranscriptLandingProbe: real JSONL scan past the offset ------------

    #[test]
    fn transcript_probe_scans_user_records_past_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let head = r#"{"type":"user","message":{"content":"OLD before offset"}}"#;
        let body = format!(
            "{head}\n{}\n{}\n",
            r#"{"type":"assistant","message":{"content":"noise"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"the landed message"}]}}"#,
        );
        std::fs::write(&path, &body).unwrap();
        let probe = TranscriptLandingProbe;
        // Offset past the head record → only the later user record is in scope.
        let offset = (head.len() + 1) as u64;
        assert_eq!(
            probe.scan(Some(path.to_str().unwrap()), Some(offset), "the landed message"),
            LandingScan::Landed
        );
        // A message that only appears BEFORE the offset is not confirmed.
        assert_eq!(
            probe.scan(Some(path.to_str().unwrap()), Some(offset), "OLD before offset"),
            LandingScan::Unconfirmed
        );
        // No transcript key → unconfirmed (cannot confirm), never a false landed.
        assert_eq!(probe.scan(None, None, "x"), LandingScan::Unconfirmed);
    }

    // ---- QS-1: keystrokes buffered during the fire flush in order on unlock -

    #[test]
    fn keystrokes_during_fire_buffer_and_flush_in_order() {
        let screen = format!("{PROMPT_GLYPH} hello");
        let fx = Arc::new(FakeFx::new(vec![screen.as_str()], vec![Some("busy")]));
        let (_d, spool) = scratch_spool();
        let lock = Arc::new(Mutex::new(InputLock::new()));
        let journal = Mutex::new(Journal::new());
        // Simulate a keystroke arriving mid-fire: admit while locked buffers it.
        // (Here we arm the lock, admit, then let fire() run its course — fire()
        // itself locks idempotently and drains on unlock.)
        lock.lock().unwrap().lock();
        assert_eq!(
            lock.lock().unwrap().admit(b"mid"),
            crate::attended::Admit::Buffered
        );
        let out = fire(
            &*fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        assert!(matches!(out, FireOutcome::Terminal(Payload::MessageSeen { .. })));
        // The buffered "mid" keystroke was flushed (written raw) on unlock, in order,
        // never lost.
        let flushed: Vec<u8> = fx.raw_writes().concat();
        assert!(
            flushed.windows(3).any(|w| w == b"mid"),
            "buffered keystroke flushed on unlock: {flushed:?}"
        );
        assert!(!lock.lock().unwrap().is_locked());
    }

    // ======================================================================
    // M4 — per-harness facts (codex + pi), verified from the observable shapes.
    // ======================================================================

    fn fire_with(
        facts: &dyn HarnessFacts,
        probe: &dyn LandingProbe,
        screens: Vec<&str>,
        statuses: Vec<Option<&str>>,
        message: &str,
    ) -> (FireOutcome, FakeFx) {
        // FakeFx is not Clone; run the fire against a local fx and return raw writes
        // via a second probe of the same fx. Simpler: build fx, run, return both.
        let fx = FakeFx::new(screens, statuses);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx, facts, probe, &lock, &journal, &spool, rec(), message, &cfg_fast(),
        );
        (out, fx)
    }

    // ---- codex glyph is U+203A (bytes e2 80 ba), NOT ❯ U+276F --------------

    #[test]
    fn codex_glyph_bytes_are_u203a() {
        assert_eq!("\u{203a}".as_bytes(), &[0xe2, 0x80, 0xba]);
        assert_eq!(CODEX_PROMPT_GLYPH, '\u{203a}');
        assert_ne!(CODEX_PROMPT_GLYPH, PROMPT_GLYPH, "codex glyph is not ❯");
        assert_eq!(CodexFacts.composer_region(), ComposerRegion::GlyphAnchor('\u{203a}'));
    }

    // ---- codex composer_is_plain: STATUS-LINE discriminator (QS-5) ----------

    #[test]
    fn codex_plain_requires_status_line_not_glyph_presence() {
        let f = CodexFacts;
        // Plain: status line "<model> <effort> · <cwd>" present AND the glyph.
        let plain = "gpt-5.6-sol default \u{00b7} ~/work/quorum\n\u{203a} Summarize recent commits";
        assert_eq!(f.composer_is_plain(plain), Some(true));
        // The /model modal — its selection marker is ALSO › (glyph-presence would
        // FALSE-POSITIVE it as plain); the status-line discriminator + header
        // signature catch it: provably NOT plain.
        let modal = "Select Model and Effort\n\u{203a} 1. gpt-5.6-sol (current)\n\u{203a} 2. gpt-5.6\nPress enter to confirm or esc to go back";
        assert_eq!(f.composer_is_plain(modal), Some(false), "the /model modal is NOT plain");
        // The double-Esc backtrack pager (Enter EDITS a previous message).
        let pager = "~\n~\n\u{203a} old message\nq to quit \u{00b7} esc/← to edit prev";
        assert_eq!(f.composer_is_plain(pager), Some(false), "the backtrack pager is NOT plain");
        // A glyph but NO status line (the transient "esc again to edit" hint
        // replaced it) → UNKNOWN → honest verify-blocked, never a blind type.
        let no_status = "\u{203a} Write tests for @filename\nesc again to edit previous message";
        assert_eq!(f.composer_is_plain(no_status), None);
    }

    // ---- codex fire: repeated Ctrl-U clear, region content-verify, message-seen

    #[test]
    fn codex_fire_repeats_ctrl_u_and_lands_message_seen() {
        let screen = "gpt-5.6-sol default \u{00b7} ~/work\n\u{203a} hello";
        let (out, fx) = fire_with(
            &CodexFacts,
            &FixedProbe(LandingScan::Landed),
            vec![screen],
            vec![Some("busy")],
            "hello",
        );
        assert_eq!(
            out,
            FireOutcome::Terminal(Payload::MessageSeen {
                send_id: "p-1".into(),
                content_sha256: quorum_delivery_events::sha256_hex(b"hello"),
            })
        );
        // The clear pressed Ctrl-U (0x15) exactly CODEX_CLEAR_PRESSES times (a
        // bounded converge; over-press is a safe no-op on empty).
        let ctrl_u = fx.raw_writes().iter().filter(|w| w.as_slice() == b"\x15").count();
        assert_eq!(ctrl_u as u32, CODEX_CLEAR_PRESSES, "repeated Ctrl-U converge");
    }

    // ---- codex QS-5: firing into the /model modal is verify-blocked, no writes

    #[test]
    fn codex_modal_is_verify_blocked_no_clear_no_inject() {
        let modal = "Select Model and Effort\n\u{203a} 1. gpt-5.6-sol (current)\nPress enter to confirm or esc to go back";
        let (out, fx) = fire_with(
            &CodexFacts,
            &FixedProbe(LandingScan::Landed),
            vec![modal],
            vec![Some("idle")],
            "hello",
        );
        match out {
            FireOutcome::Terminal(Payload::SendFailed { reason, .. }) => {
                assert_eq!(reason, "verify-blocked")
            }
            other => panic!("expected verify-blocked, got {other:?}"),
        }
        assert!(fx.raw_writes().is_empty(), "no clear-chord into a modal");
        assert!(fx.texts.lock().unwrap().is_empty(), "no inject into a modal");
    }

    // ---- SafeDefaultFacts composer_is_plain: MODAL discriminator (QS-5, O1) ----
    // claude's `/model` modal KEEPS `❯` on screen — the composer line `❯ /model`
    // PERSISTS and the dialog's selection marker is `❯ 6. Opus ✔` — so
    // glyph-presence ALONE false-positived the open modal as plain (O1 letter
    // gap: the fire could clear + TYPE into it). PRIMARY SOURCE for the modal
    // chrome asserted here: a live claude 2.1.207 TUI driven to `/model`, raw pane
    // captured 2026-07-13. MUTATION EVIDENCE: reverting SafeDefaultFacts to the
    // glyph-only body reds the modal assertion below.
    #[test]
    fn safe_default_claude_model_modal_is_not_plain_despite_glyph() {
        let f = SafeDefaultFacts;
        // A REAL plain composer (glyph present, NO modal chrome) → Some(true) [unchanged].
        let plain = format!(
            "{PROMPT_GLYPH} Try \"how does <filepath> work?\"\n  \u{23f5}\u{23f5} bypass permissions on \u{00b7} \u{2190} for agents   \u{25cf} high \u{00b7} /effort"
        );
        assert_eq!(
            f.composer_is_plain(&plain),
            Some(true),
            "a real plain claude composer stays plain"
        );
        // The `/model` modal: `❯` PERSISTS (composer `❯ /model` + marker `❯ 6. Opus ✔`),
        // so the signature discriminator — NOT the glyph — must catch it.
        let modal = format!(
            "{PROMPT_GLYPH} /model\n\n  Select model\n  Switch between Claude models. Your pick becomes the default for new sessions.\n\n  {PROMPT_GLYPH} 6. Opus \u{2714}                 Opus 4.8 \u{00b7} Best for everyday, complex tasks\n\n  Enter to set as default \u{00b7} s to use this session only \u{00b7} Esc to cancel"
        );
        // Guard: the captured modal DOES contain `❯` — the exact collision O1 is about.
        assert!(
            modal.contains(PROMPT_GLYPH),
            "the modal capture must contain ❯ (the glyph collision O1 addresses)"
        );
        assert_eq!(
            f.composer_is_plain(&modal),
            Some(false),
            "claude /model modal is NOT plain despite the persisting ❯ (QS-5/O1)"
        );
        // No glyph, no modal chrome → UNKNOWN (fail-safe) [unchanged].
        assert_eq!(f.composer_is_plain("a bare palette, no glyph"), None);
    }

    // ---- SafeDefaultFacts QS-5/O1: firing into the claude /model modal is
    // verify-blocked — no clear-chord, no inject (mirrors the codex modal test).
    // MUTATION EVIDENCE: reverting SafeDefaultFacts to glyph-only makes this fire
    // clear + inject into the modal (raw_writes/texts non-empty) → reds this.
    #[test]
    fn safe_default_claude_model_modal_is_verify_blocked_no_clear_no_inject() {
        let modal = format!(
            "{PROMPT_GLYPH} /model\n  Select model\n  Switch between Claude models. Your pick becomes the default for new sessions.\n  {PROMPT_GLYPH} 6. Opus \u{2714}\n  Enter to set as default \u{00b7} s to use this session only \u{00b7} Esc to cancel"
        );
        let fx = FakeFx::new(vec![modal.as_str()], vec![Some("idle")]);
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        match out {
            FireOutcome::Terminal(Payload::SendFailed { reason, .. }) => {
                assert_eq!(reason, "verify-blocked")
            }
            other => panic!("expected verify-blocked, got {other:?}"),
        }
        assert!(fx.raw_writes().is_empty(), "no clear-chord into the claude /model modal");
        assert!(fx.texts.lock().unwrap().is_empty(), "no inject into the claude /model modal");
        assert_eq!(fx.crs.load(Ordering::SeqCst), 0, "no CR into the modal");
        assert!(!lock.lock().unwrap().is_locked(), "lock released, not wedged");
    }

    // ---- pi glyph is U+2500 rule (bytes e2 94 80); region is between two rules

    #[test]
    fn pi_rule_bytes_are_u2500_and_region_is_between_two() {
        assert_eq!("\u{2500}".as_bytes(), &[0xe2, 0x94, 0x80]);
        assert_eq!(PI_RULE, '\u{2500}');
        assert_eq!(PiFacts.composer_region(), ComposerRegion::BetweenLastTwo('\u{2500}'));
    }

    // ---- pi composer_is_plain: two rules + no → marker -----------------------

    fn pi_frame(composer: &str) -> String {
        let rule: String = std::iter::repeat('\u{2500}').take(60).collect();
        format!("{rule}\n{composer}\n{rule}\n~/work (branch)  $0.000 gpt-5.5 \u{00b7} medium\n")
    }

    #[test]
    fn pi_plain_requires_two_rules_and_no_selection_marker() {
        let f = PiFacts;
        assert_eq!(f.composer_is_plain(&pi_frame("q7 probe pi alpha")), Some(true));
        assert_eq!(f.composer_is_plain(&pi_frame("")), Some(true), "empty pi composer still plain");
        // The /model overlay: a → selection marker present ⇒ NOT plain.
        let overlay = "> search\n\u{2192} 1. gpt-5.5\n\u{2192} 2. gpt-5.6\n\u{2500}\u{2500}\u{2500}\u{2500}";
        assert_eq!(f.composer_is_plain(overlay), Some(false), "the model overlay is NOT plain");
        // Fewer than two rules → UNKNOWN → honest verify-blocked.
        let one_rule = format!("q7\n{}\nfooter", "\u{2500}".repeat(60));
        assert_eq!(f.composer_is_plain(&one_rule), None);
    }

    // ---- pi fire: SINGLE Ctrl-C clear (never a rapid second), message-seen ----

    #[test]
    fn pi_fire_single_ctrl_c_and_lands_message_seen() {
        // `rec()` keys the spool record to "hello"; the MessageSeen carries the
        // RECORD's sha, so the message must be "hello" for the terminal to match.
        let screen = pi_frame("hello");
        let (out, fx) = fire_with(
            &PiFacts,
            &FixedProbe(LandingScan::Landed),
            vec![screen.as_str()],
            vec![Some("busy")],
            "hello",
        );
        assert_eq!(
            out,
            FireOutcome::Terminal(Payload::MessageSeen {
                send_id: "p-1".into(),
                content_sha256: quorum_delivery_events::sha256_hex(b"hello"),
            })
        );
        // EXACTLY ONE Ctrl-C (0x03) — a rapid second would EXIT pi.
        let ctrl_c = fx.raw_writes().iter().filter(|w| w.as_slice() == b"\x03").count();
        assert_eq!(ctrl_c, 1, "pi clears with a SINGLE Ctrl-C, never a rapid second");
        // The clear-chord bytes never contain Ctrl-D (0x04, exits pi on empty).
        assert!(
            !fx.raw_writes().iter().any(|w| w.contains(&0x04u8)),
            "never emit Ctrl-D to pi"
        );
    }

    // ---- codex LandingProbe: parses a codex rollout record --------------------

    #[test]
    fn codex_landing_probe_parses_rollout_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        let body = format!(
            "{}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"noise"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with exactly: q7-ok"}]}}"#,
        );
        std::fs::write(&path, &body).unwrap();
        let probe = CodexLandingProbe;
        assert_eq!(
            probe.scan(Some(path.to_str().unwrap()), Some(0), "Reply with exactly: q7-ok"),
            LandingScan::Landed
        );
        // The DEFAULT (claude/pi) probe can NEVER match a codex rollout.
        assert_eq!(
            TranscriptLandingProbe.scan(Some(path.to_str().unwrap()), Some(0), "Reply with exactly: q7-ok"),
            LandingScan::Unconfirmed,
            "the claude/pi probe cannot match a codex rollout shape"
        );
    }

    // ---- pi LandingProbe: the DEFAULT probe now matches pi records ------------

    #[test]
    fn default_probe_matches_pi_message_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.jsonl");
        let body = format!(
            "{}\n{}\n",
            r#"{"type":"message","id":"a","message":{"role":"assistant","content":[{"type":"text","text":"a reply"}]}}"#,
            r#"{"type":"message","id":"b","parentId":"a","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly: q7-pi-ok"}]}}"#,
        );
        std::fs::write(&path, &body).unwrap();
        let probe = TranscriptLandingProbe;
        assert_eq!(
            probe.scan(Some(path.to_str().unwrap()), Some(0), "Reply with exactly: q7-pi-ok"),
            LandingScan::Landed,
            "pi's type==message&&role==user record lands via the default probe"
        );
        // The assistant record is not a user landing.
        assert_eq!(
            probe.scan(Some(path.to_str().unwrap()), Some(0), "a reply"),
            LandingScan::Unconfirmed,
            "an assistant record is not a user landing"
        );
    }

    // ---- clear degradation: non-plain after clear ⇒ verify-blocked (safe floor)

    #[test]
    fn clear_leaving_nonplain_composer_degrades_verify_blocked() {
        // Screen is plain at the pre-clear verify (first read), then a modal on the
        // reverify read (second) — the clear left a non-plain composer. Codex facts
        // reverify_plain=true ⇒ verify-blocked, NEVER a blind inject.
        let plain = "gpt-5.6 default \u{00b7} ~/work\n\u{203a} hello";
        let modal = "Select Model and Effort\nPress enter to confirm or esc to go back";
        let (out, fx) = fire_with(
            &CodexFacts,
            &FixedProbe(LandingScan::Landed),
            vec![plain, modal], // read #1 (pre-clear verify) plain; read #2+ (reverify) modal
            vec![Some("idle")],
            "hello",
        );
        match out {
            FireOutcome::Terminal(Payload::SendFailed { reason, .. }) => {
                assert_eq!(reason, "verify-blocked")
            }
            other => panic!("expected verify-blocked after non-plain reverify, got {other:?}"),
        }
        // The clear DID press Ctrl-U (composer disturbed), but the inject never ran.
        assert!(fx.texts.lock().unwrap().is_empty(), "no inject after a failed reverify");
    }

    // ---- M5/T6: LANDING-as-acceptance un-gates codex delivery ---------------

    /// A `AcceptanceSignal::Landing` harness (codex) FIRES — it is no longer gated
    /// off — and its acceptance comes from the transcript, not from a busy status
    /// it never publishes.
    ///
    /// Note the fixture: `confirmable = false` (codex genuinely has no status
    /// source) and every `read_status()` is `None`. Under the old contract that
    /// combination meant "cannot deliver"; the whole point of the un-gate is that
    /// it now means "ask the transcript instead".
    ///
    /// MUTATION EVIDENCE: reverting the gate to `if !effects.acceptance_confirmable()`
    /// reds this — the fire returns `acceptance-unconfirmable` with no inject.
    #[test]
    fn landing_harness_fires_and_confirms_from_the_transcript() {
        let plain = "gpt-5.6-sol default \u{00b7} ~/work\n\u{203a} hello";
        let fx = FakeFx::new(vec![plain], vec![None, None, None]);
        *fx.confirmable.lock().unwrap() = false; // codex: no status source, ever
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &CodexFacts,
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        match out {
            FireOutcome::Terminal(Payload::MessageSeen { .. }) => {}
            other => panic!("a landed codex send must be MessageSeen, got {other:?}"),
        }
        // It genuinely delivered: the text went out and exactly one CR submitted it.
        assert_eq!(
            fx.texts.lock().unwrap().concat(),
            "hello",
            "the message must actually be injected"
        );
        assert_eq!(
            fx.crs.load(Ordering::SeqCst),
            1,
            "exactly ONE CR — the busy-keyed remediation must not run without a status signal"
        );
    }

    /// THE double-submit guarantee, stated as its own test because it is the whole
    /// reason the F1 gate existed.
    ///
    /// With no status source, `wait_for_busy` can never return true, so the
    /// discipline's acceptance-keyed remediation would fire a SECOND CR on every
    /// single send — submitting twice whenever the first CR worked. A Landing
    /// harness therefore takes the two-write shape with ONE CR and lets the
    /// transcript decide.
    ///
    /// MUTATION EVIDENCE: routing Landing through `deliver_idle_two_write_in_region`
    /// (the BusyTransition path) reds this with 2 CRs.
    #[test]
    fn landing_harness_never_fires_a_second_cr() {
        let plain = "gpt-5.6-sol default \u{00b7} ~/work\n\u{203a} hello";
        // Landing is never confirmed AND status is always None — the worst case for
        // a remediation loop keyed on "did we go busy?".
        let fx = FakeFx::new(vec![plain], vec![None, None, None, None]);
        *fx.confirmable.lock().unwrap() = false;
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &CodexFacts,
            &FixedProbe(LandingScan::Unconfirmed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        assert_eq!(
            fx.crs.load(Ordering::SeqCst),
            1,
            "one CR even when nothing ever confirms — never a blind second submit"
        );
        // And an unconfirmed landing is NOT a false delivery and NOT a false death:
        // it stays Pending for --wait/reconcile to resolve.
        assert!(
            matches!(out, FireOutcome::Pending),
            "unconfirmed landing stays Pending, got {out:?}"
        );
    }

    /// A Landing harness must NOT be reported `recipient-gone` just because it has
    /// no status source.
    ///
    /// `landing_terminal` infers a dead session from `read_status() == None`, which
    /// is sound only where a status source exists. codex reads `None` always and by
    /// design, so without scoping that inference EVERY codex send would terminate
    /// as a false death on the first poll — the normal "rollout not flushed yet"
    /// state misreported as a dead recipient.
    ///
    /// MUTATION EVIDENCE: dropping the `signal == BusyTransition` guard reds this
    /// with a `SeenFailed{recipient-gone}` terminal.
    #[test]
    fn landing_harness_unconfirmed_is_not_reported_as_a_dead_recipient() {
        let plain = "gpt-5.6-sol default \u{00b7} ~/work\n\u{203a} hello";
        let fx = FakeFx::new(vec![plain], vec![None, None, None, None]);
        *fx.confirmable.lock().unwrap() = false;
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        let out = fire(
            &fx,
            &CodexFacts,
            &FixedProbe(LandingScan::Unconfirmed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );
        match out {
            FireOutcome::Terminal(Payload::SeenFailed { reason, .. }) => {
                panic!("healthy codex session misreported as gone: {reason}")
            }
            FireOutcome::Pending => {}
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    /// The claude path must be untouched by all of this: it keeps the
    /// busy-transition signal and its content-verified remediation CR.
    #[test]
    fn busy_transition_harness_keeps_its_remediation_cr() {
        assert_eq!(
            SafeDefaultFacts.acceptance_signal(),
            AcceptanceSignal::BusyTransition,
            "the default harness keeps the busy-transition signal"
        );
        assert_eq!(
            CodexFacts.acceptance_signal(),
            AcceptanceSignal::Landing,
            "codex confirms from its rollout"
        );
        assert_eq!(
            PiFacts.acceptance_signal(),
            AcceptanceSignal::Landing,
            "pi confirms from its session transcript — append-per-entry once flushed, \
             NOT the append-on-exit the M5 note recorded"
        );
    }

    // ---- F1: NEITHER acceptance signal ⇒ NO clear/inject, composer UNTOUCHED,
    //          honest non-delivery terminal (never a false delivery) -------------

    /// A harness with NEITHER acceptance signal is gated OFF before any
    /// clear/inject: composer UNTOUCHED, honest non-delivery, never a false
    /// delivery and never a dangling Pending.
    ///
    /// pi-interactive: SCOPED TO A SYNTHETIC HARNESS, and deliberately so. This
    /// test ran over codex, then over pi alone; both have since been shown to
    /// record the submitted message and moved to [`AcceptanceSignal::Landing`], so
    /// NO SHIPPED HARNESS closes this gate today. Deleting the coverage would
    /// leave the gate — the thing standing between us and injecting turns we can
    /// never observe accepted — completely untested, so the subject is now an
    /// explicit stand-in for the next carrier that arrives with neither signal:
    /// claude-shaped composer facts, no status source.
    ///
    /// MUTATION EVIDENCE: dropping the `!effects.acceptance_confirmable()` half of
    /// the gate reds this — the fire proceeds to clear + inject + CR.
    #[test]
    fn gate_closes_on_a_harness_with_neither_signal() {
        // Neither signal = the default `BusyTransition` acceptance (no Landing
        // override) AND `confirmable = false` (no status source to observe it on).
        struct NeitherSignalFacts;
        impl HarnessFacts for NeitherSignalFacts {
            fn clear_chord(&self) -> Vec<u8> {
                SafeDefaultFacts.clear_chord()
            }
            fn composer_is_plain(&self, screen_text: &str) -> Option<bool> {
                SafeDefaultFacts.composer_is_plain(screen_text)
            }
            // acceptance_signal(): the trait default, BusyTransition.
        }
        assert_eq!(
            NeitherSignalFacts.acceptance_signal(),
            AcceptanceSignal::BusyTransition,
            "the stand-in must have no Landing override — that is the point"
        );

        // A LIVE-SHAPED PLAIN composer, so it is the GATE and not the plain-verify
        // that stops the fire.
        let screen = "\u{276f} hello";
        assert_eq!(
            NeitherSignalFacts.composer_is_plain(screen),
            Some(true),
            "live-shaped composer is plain (the pre-gate fire would proceed to inject)"
        );

        let fx = FakeFx::new(vec![screen], vec![Some("busy")]);
        *fx.confirmable.lock().unwrap() = false; // no status source
        let (_d, spool) = scratch_spool();
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        // Seed a human draft: it must be preserved untouched (no clear/re-show).
        let mut j = Journal::new();
        j.on_human_input(b"human draft", 0, 11);
        *journal.lock().unwrap() = j;

        let out = fire(
            &fx,
            &NeitherSignalFacts,
            // Scripted Landed and STILL unreachable — the gate returns first.
            &FixedProbe(LandingScan::Landed),
            &lock,
            &journal,
            &spool,
            rec(),
            "hello",
            &cfg_fast(),
        );

        match out {
            FireOutcome::Terminal(Payload::SendFailed { reason, send_id, .. }) => {
                assert_eq!(
                    reason, "acceptance-unconfirmable",
                    "the F1 gate's distinct reason, not the plain-verify's"
                );
                assert_eq!(send_id.as_deref(), Some("p-1"), "non-dangling, keyed");
            }
            other => panic!("expected honest SendFailed, got {other:?}"),
        }
        // Composer UNTOUCHED: no clear-chord, no inject text, no CR.
        assert!(fx.raw_writes().is_empty(), "no clear-chord / no re-show write");
        assert!(fx.texts.lock().unwrap().is_empty(), "no inject bytes");
        assert_eq!(fx.crs.load(Ordering::SeqCst), 0, "no CR");
    }
}
