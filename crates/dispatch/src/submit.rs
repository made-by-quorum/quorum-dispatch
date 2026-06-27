//! TS fix-wave submit-discipline fix A (TS-internal codename redacted per engine
//! scope-audit; see qa/hardening@3dd9f1e:src/commands/submit.ts:1) —
//! acceptance-keyed verify-then-CR submit discipline.
//!
//! EXACT port of `qa/hardening@3dd9f1e:src/commands/submit.ts:1-135` (whole file;
//! byte-identical across local qa/hardening, remote qa/hardening, and the sendpty
//! tip — see a4-spec §0). The TS file-header war-story comments port VERBATIM
//! below (LESSONS.md comments-carry rule; the `commands/` path prefix is preserved
//! per a prior red-team that flagged dropped prefixes).
//!
//! The bounded-retry `deliver_prompt` wrapper (a4-spec §2.2) is ported from
//! `qa/hardening@3dd9f1e:src/commands/lifecycle.ts:265-345` and lands here too,
//! adapted to the THREE-WAY [`DeliverOutcome`] the went-busy exit contract needs
//! (spec-red-team R1: a vanished PID file is infra failure, not a stall).
//!
//! ---------------------------------------------------------------------------
//! VERBATIM file-header (qa/hardening@3dd9f1e:src/commands/submit.ts:1-26;
//! TS-internal codename redacted per engine scope-audit — RATIONALE fidelity, not
//! name-byte fidelity, per the comments-carry ruling):
//!
//! TS fix-wave fix A — acceptance-keyed verify-then-CR submit discipline.
//!
//! Evidence: doc/log/2026-05-29_p0-spike.md §F1 + FINDING-E1 (P0 spike).
//! Contract: doc/spec/qd-spawn-contract.md §4 SUBMIT DISCIPLINE block.
//!
//! `qd new -p` and `qd send:pty` deliver a prompt as a SINGLE PTY write of
//! `message + "\r"`. A large single write (≳~400 chars on the test setup) — and,
//! on a freshly-booted session, sometimes even a short one (FINDING-E1) — is
//! caught by paste-burst detection so its trailing `\r` is absorbed as a literal
//! newline in the composer instead of submitting. The text sits unsubmitted.
//!
//! The robust rule is NOT to predict whether a paste will submit. Instead, after
//! any delivery, verify the session was ACCEPTED — i.e. it went `busy` — within a
//! short settle window (~2s, chosen to exceed boot-to-accept latency measured at
//! ~1.5s and NOT to scale with response length). Only if it did NOT go busy in
//! that window, emit EXACTLY ONE `zmx send <name> $'\r'` and re-check for busy.
//!
//! Load-bearing invariants:
//!  - Key on ACCEPTANCE (busy), never on COMPLETION (turns-advance / busy→idle):
//!    a long first response stays busy ~15s; a completion-keyed check would read a
//!    still-running response as "not submitted" and fire a stray CR into a busy
//!    session — the exact spurious empty turn this rule prevents.
//!  - NEVER send a CR to a session that is already busy (raw `zmx send` does not
//!    refuse a busy session the way `qd send` does — commands/send.ts:94 — so the
//!    busy guard is explicit here).
//!  - NEVER blanket-CR: on a prompt that already submitted, a second CR injects a
//!    second submit → a spurious empty turn. At most ONE CR, only when not busy.
//! ---------------------------------------------------------------------------

use crate::boot::{find_pid_file, read_pid_status, Sleeper};
use crate::effects::Clock;
use crate::mux::Mux;
use crate::sendpty::composer_holds_message;
use std::path::PathBuf;

/// Status string that means the session ACCEPTED the turn
/// (qa/hardening@3dd9f1e:src/commands/submit.ts:65 `const BUSY = "busy"`).
const BUSY: &str = "busy";

/// Effects the discipline needs, injected so the logic is unit-testable
/// (qa/hardening@3dd9f1e:src/commands/submit.ts:29-39 `interface SubmitDeps`).
///
/// `read_status` returns `None` when the status is unreadable (PID file gone) —
/// the TS `string | undefined`. `now_ms` is the monotonic-ish clock.
pub trait SubmitDeps {
    /// Current session status, or `None` if unreadable (e.g. PID file gone).
    fn read_status(&self) -> Option<String>;
    /// Send exactly one carriage return to the session (`zmx send <name> $'\r'`).
    fn send_cr(&self);
    /// Sleep this many ms. Injected so tests run instantly.
    fn sleep(&self, ms: u64);
    /// Monotonic clock in ms. Injected so tests are deterministic.
    fn now_ms(&self) -> i64;
}

/// Tunable windows (qa/hardening@3dd9f1e:src/commands/submit.ts:41-52
/// `interface SubmitOptions`). Defaults applied via [`SubmitOptions::default`].
#[derive(Debug, Clone, Copy)]
pub struct SubmitOptions {
    /// Acceptance settle window in ms (default 2500 — comfortably exceeds the
    /// ~1.5s boot-to-accept latency with slow-load margin; r1 nit F7). Separates
    /// an accepted prompt from a stuck composer without scaling with response
    /// length.
    pub settle_ms: u64,
    /// Window in ms to wait for busy AFTER the remediation CR (default 13000).
    pub post_cr_ms: u64,
    /// Status poll interval in ms (default 250).
    pub poll_ms: u64,
}

impl Default for SubmitOptions {
    fn default() -> Self {
        Self {
            settle_ms: 2500,
            post_cr_ms: 13_000,
            poll_ms: 250,
        }
    }
}

/// Result of [`verify_accepted_then_cr`]
/// (qa/hardening@3dd9f1e:src/commands/submit.ts:54-63 `interface SubmitOutcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// Did the session reach `busy` (accepted)?
    pub accepted: bool,
    /// How many remediation CRs were fired (0 if it auto-submitted, else 1).
    pub crs_fired: u32,
    /// True if a CR was suppressed because the session was already busy.
    pub suppressed_busy_cr: bool,
}

/// Poll `read_status` until it reports `busy`, the window elapses, or the status
/// source disappears. Returns true iff `busy` was observed within the window.
/// Port of `waitForBusy` (qa/hardening@3dd9f1e:src/commands/submit.ts:67-82).
fn wait_for_busy(deps: &dyn SubmitDeps, window_ms: u64, poll_ms: u64) -> bool {
    let deadline = deps.now_ms() + window_ms as i64;
    // Check immediately, then poll, so a ~0ms window still does one read.
    loop {
        if deps.read_status().as_deref() == Some(BUSY) {
            return true;
        }
        if deps.now_ms() >= deadline {
            return false;
        }
        // Sleep min(poll_ms, max(0, deadline - now)) so the window is never
        // overshot (TS `Math.min(pollMs, Math.max(0, deadline - deps.now()))`,
        // submit.ts:80).
        let remaining = (deadline - deps.now_ms()).max(0) as u64;
        deps.sleep(poll_ms.min(remaining));
    }
}

/// The acceptance-keyed verify-then-CR core. Assumes the caller has ALREADY
/// issued the single `message + "\r"` PTY write; this verifies acceptance and
/// remediates exactly once if needed. Port of `verifyAcceptedThenCR`
/// (qa/hardening@3dd9f1e:src/commands/submit.ts:89-135).
pub fn verify_accepted_then_cr(deps: &dyn SubmitDeps, opts: SubmitOptions) -> SubmitOutcome {
    // Phase 1 — settle window: did the trailing \r auto-submit (session went busy)?
    if wait_for_busy(deps, opts.settle_ms, opts.poll_ms) {
        return SubmitOutcome {
            accepted: true,
            crs_fired: 0,
            suppressed_busy_cr: false,
        };
    }

    // Not busy within the settle window. Re-read once more right before acting, so
    // we never CR a session that just crossed into busy at the window boundary
    // (defends D-never-cr-busy / the long-response false-negative).
    if deps.read_status().as_deref() == Some(BUSY) {
        return SubmitOutcome {
            accepted: true,
            crs_fired: 0,
            suppressed_busy_cr: true,
        };
    }

    // Composer is stuck (paste-burst absorbed the \r, or just-booted fragility).
    // Emit EXACTLY ONE carriage return to submit it.
    deps.send_cr();

    // Re-check for busy after the remediation CR.
    let accepted = wait_for_busy(deps, opts.post_cr_ms, opts.poll_ms);
    SubmitOutcome {
        accepted,
        crs_fired: 1,
        suppressed_busy_cr: false,
    }
}

// ===========================================================================
// R4 FIX — two-write delivery + content-verified-CR on the IDLE path
// (ADR 0009; orc-2 RULED fix-in-phase, ruling relay-1780631655040-9 item 2).
//
// LIVE EVIDENCE (test/golden/dryrun/a4-live-evidence.md §FINDING +
// a4-paste-bytes.txt; soak-ledger R4 row): on REAL claude 2.1.163, a ≥~4KB single
// PTY write of `message + "\r"` on the IDLE send:pty path is paste-burst-absorbed
// (the trailing \r becomes a literal newline; the message sits UNSUBMITTED in the
// composer) — and in a 2-boot reproduction even the discipline's ONE remediation
// CR did NOT recover it live. TS has the IDENTICAL single-write idle mechanism
// (0d0fa9e:src/commands/send.ts:204 `sendRaw(message + "\r")`), so TS production
// loses such pastes today. This fix is a SANCTIONED divergence under ADD-9a (never
// reproduce a TS bug); the upstream report is filed at the A4 merge ruling.
//
// The fix MIRRORS the already-proven queue path (send.ts:154-180): deliver as TWO
// writes — text alone, ~200ms settle, "\r" alone (the human-keystroke shape a
// paste burst does NOT collapse) — and remediate with a CONTENT-VERIFIED CR (read
// the composer; CR only while it provably still holds OUR exact text). The
// acceptance-keyed verify-then-CR (an idle session must go busy) is PRESERVED; only
// its remediation CR becomes content-verified (never blind). All load-bearing
// invariants stay: never-CR-busy, ≤1 remediation CR, keyed on ACCEPTANCE.
//
// This helper is the SINGLE production delivery mechanism the idle bin path
// (verbs/send.rs) and the fakerepl R4 gate row BOTH drive — the test exercises
// real code, not a reimplementation (a4-r4 deliverable #4).
// ===========================================================================

/// The settle between the two writes (text, THEN a separate "\r"), ms. Mirrors the
/// queue path's 200ms (qa/hardening@3dd9f1e:src/commands/send.ts:164,178) — long
/// enough that the text burst closes (>GAP) before the CR arrives, so the CR is its
/// OWN non-paste keystroke burst.
pub const TWO_WRITE_SETTLE_MS: u64 = 200;

// ===========================================================================
// Chunked PTY text delivery — the tty-queue-OVERFLOW fix (ADR 0009 mode (a)).
//
// PARITY PORT of `8c59ec4:src/commands/submit.ts:140-230` (chunkText +
// sendTextChunked) — this is parity with the new TS reference, NOT a divergence.
//
// TWO DISTINCT, size-gated failure modes hit a single large `zmx send` of a prompt
// (ADR 0009); both are now defended:
//
//  (a) PTY INPUT-BUFFER OVERFLOW (the wholesale-drop). A single `zmx send` of a
//      large payload overflows the canonical tty input queue (~4096B) BEFORE
//      claude's reader drains it, so the write is DISCARDED WHOLESALE — the text
//      never reaches the composer at all, the composer stays EMPTY, and the
//      content-gated remediation CR correctly fires nothing (there is nothing to
//      submit). This is UPSTREAM of the submit discipline. LIVE on the Rust
//      send:pty idle path: 8KB DELIVERED, 12KB and 16KB EMPTY-DROPPED (delta 0,
//      did-not-go-busy WARNING, marker absent from the composer —
//      test/golden/dryrun/a4-r6-probe-evidence.md, ordered relay-1780637708238-13).
//      The observed boundary (8KB clean / ≥12KB dropped on brano; TS observed ~4KB)
//      is MACHINE/LOAD-DEPENDENT — it is EVIDENCE, never a constant in code. The
//      INVARIANT is the chunk size. FIX: chunk_text + send_text_chunked below — split
//      the text into ≤chunk_bytes (default 1024B) code-point-safe chunks, send each
//      as its own write with a ~150ms inter-chunk delay so claude's reader drains the
//      queue between writes, THEN the existing two-write CR discipline on top.
//
//  (b) PASTE-BURST \r ABSORPTION (the earlier R4 finding). A trailing \r written
//      WITHIN the same paste burst as the text is absorbed as a literal newline and
//      never submits. FIX: two-write delivery (text, settle, then \r ALONE) so the
//      \r lands as its own non-paste keystroke. Unchanged; the two-write CR discipline
//      still runs AFTER the chunked text.
// ===========================================================================

/// Default inter-chunk settle (ms) so claude's reader drains the tty queue between
/// chunk writes (8c59ec4:src/commands/submit.ts:181 `interChunkMs = 150`). Applied
/// BETWEEN chunks only — not before the first, not after the last.
pub const CHUNK_SETTLE_MS: u64 = 150;

/// Default max BYTES per chunk (8c59ec4:src/commands/submit.ts:170 `chunkBytes =
/// 1024`), kept well under the ~4096B canonical tty queue with margin. A chunk NEVER
/// splits a UTF-8 code point, so an individual chunk may run a few bytes under this
/// bound (8c59ec4:src/commands/submit.ts:175 comment).
pub const CHUNK_BYTES: usize = 1024;

/// Tunable chunking seams (8c59ec4:src/commands/submit.ts:165-185
/// `interface ChunkSendOptions`). Injectable so the fakerepl gate / unit rows run
/// fast; defaults are the TS-cited 1024B / 150ms via [`ChunkSendOptions::default`].
#[derive(Debug, Clone, Copy)]
pub struct ChunkSendOptions {
    /// Max BYTES per chunk (UTF-8). Default [`CHUNK_BYTES`] (1024). A chunk never
    /// splits a character, so a chunk may be a few bytes under this bound.
    pub chunk_bytes: usize,
    /// Delay between chunks, ms. Default [`CHUNK_SETTLE_MS`] (150). Not applied
    /// before the first or after the last chunk.
    pub settle_ms: u64,
}

impl Default for ChunkSendOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: CHUNK_BYTES,
            settle_ms: CHUNK_SETTLE_MS,
        }
    }
}

/// Split `message` into chunks each ≤ `chunk_bytes` UTF-8 bytes, on CODE-POINT
/// boundaries (never mid-character). PURE so the boundary behavior is unit-testable.
/// Parity port of `chunkText` (8c59ec4:src/commands/submit.ts:186-208).
///
/// CRITICAL: we iterate over `char`s (Unicode scalar values / code points), NOT raw
/// byte indices — a naive `&s[i..i+1024]` slice can fall MID-CHARACTER and either
/// panic or (in the TS surrogate-pair analogue) corrupt the text. Each code point is
/// appended whole; a new chunk starts when adding the next point would exceed the
/// byte budget. A single code point larger than `chunk_bytes` (only possible with an
/// absurdly small budget) still goes out ALONE rather than being split.
///
/// Returns `[]` for the empty string (the caller short-circuits — no write at all).
/// Properties (asserted in tests): `chunks.concat() == message` BYTE-for-byte, every
/// chunk is ≤ `chunk_bytes` (except a lone over-budget code point) and is itself
/// valid UTF-8 (trivially, since chunks are `&str`).
pub fn chunk_text(message: &str, chunk_bytes: usize) -> Vec<&str> {
    let max = chunk_bytes.max(1);
    let mut chunks: Vec<&str> = Vec::new();
    let mut start = 0usize; // byte index where the current chunk began
    let mut cur_bytes = 0usize; // bytes accumulated in the current chunk
    for (idx, ch) in message.char_indices() {
        let cp_bytes = ch.len_utf8();
        if cur_bytes != 0 && cur_bytes + cp_bytes > max {
            // Adding this code point would overflow the budget — close the current
            // chunk at `idx` (a guaranteed char boundary) and start a new one.
            chunks.push(&message[start..idx]);
            start = idx;
            cur_bytes = 0;
        }
        cur_bytes += cp_bytes;
    }
    if start < message.len() {
        chunks.push(&message[start..]);
    }
    chunks
}

/// Deliver `message` to the PTY as chunked text (NO trailing \r — the caller submits
/// separately, two-write style). Each chunk ≤ `opts.chunk_bytes` on character
/// boundaries, with an `opts.settle_ms` inter-chunk delay so the tty queue drains
/// between writes (fix (a) above). Parity port of `sendTextChunked`
/// (8c59ec4:src/commands/submit.ts:215-230).
///
/// `send` writes one chunk to the PTY (`zmx send <name> <chunk>`); `sleep` is the
/// injected inter-chunk delay. An EMPTY message is a no-op (no write at all). A
/// message that fits in ONE chunk is a SINGLE `send` with NO delay — i.e. for any
/// payload ≤ `chunk_bytes` this is BYTE-IDENTICAL to a single unchunked write, which
/// is why a slash command / small prompt routed through here is unchanged (ADR 0009:
/// the W1 single-write exception is moot by construction).
pub fn send_text_chunked(
    send: &mut dyn FnMut(&str),
    sleep: &mut dyn FnMut(u64),
    message: &str,
    opts: ChunkSendOptions,
) {
    let chunks = chunk_text(message, opts.chunk_bytes);
    for (i, chunk) in chunks.iter().enumerate() {
        if i > 0 {
            sleep(opts.settle_ms);
        }
        send(chunk);
    }
}

/// Effects the two-write idle delivery needs, injected so the mechanism is
/// unit-testable AND drivable by the fakerepl gate without a live mux/fs. The real
/// binding (verbs/send.rs) sends via `zmx send` pinned to the op dir and reads the
/// screen via `zmx history`; the test binding writes to the PTY master and reads
/// the captured app-output.
///
/// `read_status`/`now_ms`/`sleep` drive the acceptance-keyed verify-then-CR;
/// `read_screen` powers the CONTENT-VERIFIED remediation CR.
pub trait IdleDeliverDeps {
    /// Raw PTY write of `text` ALONE — no trailing CR (the first of the two
    /// writes). `zmx send <name> <text>`.
    fn send_text(&self, text: &str);
    /// Raw PTY write of a lone "\r" — the SEPARATE keystroke CR (the second write
    /// AND the remediation CR). `zmx send <name> $'\r'`.
    fn send_cr(&self);
    /// The session's screen (`zmx history`) for the content-verified CR predicate.
    fn read_screen(&self) -> String;
    /// Current session status, or `None` if unreadable (PID file gone).
    fn read_status(&self) -> Option<String>;
    /// Sleep this many ms (injected so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic clock in ms.
    fn now_ms(&self) -> i64;
}

/// Reference forwarding so a `&dyn IdleDeliverDeps` (the idle helper's param) can
/// be wrapped in the OWNED [`ContentVerifiedSubmit`] without cloning the deps.
impl<T: IdleDeliverDeps + ?Sized> IdleDeliverDeps for &T {
    fn send_text(&self, text: &str) {
        (**self).send_text(text)
    }
    fn send_cr(&self) {
        (**self).send_cr()
    }
    fn read_screen(&self) -> String {
        (**self).read_screen()
    }
    fn read_status(&self) -> Option<String> {
        (**self).read_status()
    }
    fn sleep(&self, ms: u64) {
        (**self).sleep(ms)
    }
    fn now_ms(&self) -> i64 {
        (**self).now_ms()
    }
}

/// A [`SubmitDeps`] view over an [`IdleDeliverDeps`] whose `send_cr` is
/// CONTENT-VERIFIED: it reads the composer and emits the CR ONLY while the screen
/// provably still holds OUR exact `message` unsubmitted (never blind). Wraps the
/// pure [`verify_accepted_then_cr`] core unchanged — the ≤1-CR / never-CR-busy /
/// acceptance-keyed invariants are all the core's; this only makes each remediation
/// CR conditional on the screen.
///
/// Owns its `inner` so it can be BOXED (the `deliver_prompt` per-round path returns
/// `Box<dyn SubmitDeps>`); the idle path passes a reference-wrapping inner.
pub struct ContentVerifiedSubmit<D: IdleDeliverDeps> {
    inner: D,
    message: String,
}

impl<D: IdleDeliverDeps> ContentVerifiedSubmit<D> {
    /// Wrap `inner` so its remediation CR is content-verified against `message`.
    pub fn new(inner: D, message: &str) -> Self {
        Self {
            inner,
            message: message.to_string(),
        }
    }
}

impl<D: IdleDeliverDeps> SubmitDeps for ContentVerifiedSubmit<D> {
    fn read_status(&self) -> Option<String> {
        self.inner.read_status()
    }
    fn send_cr(&self) {
        // CONTENT-VERIFIED (never BLIND): read the composer and CR only while it
        // still holds OUR exact text unsubmitted. If the message already submitted
        // (auto, or via the two-write CR), the composer no longer holds it →
        // composer_holds_message is false → NO CR (the live R4 "remediation CR
        // doesn't recover it" risk is bounded to ONLY firing when our text is
        // genuinely visible + stuck). Anchored after the LAST ❯ glyph so a
        // scrollback echo can't false-positive.
        if composer_holds_message(&self.inner.read_screen(), &self.message) {
            self.inner.send_cr();
        }
    }
    fn sleep(&self, ms: u64) {
        self.inner.sleep(ms);
    }
    fn now_ms(&self) -> i64 {
        self.inner.now_ms()
    }
}

/// Deliver `message` to an IDLE session with the R4 two-write mechanism, then the
/// acceptance-keyed CONTENT-VERIFIED verify-then-CR (ADR 0009). Returns the
/// [`SubmitOutcome`] of the acceptance check (`accepted` iff the session went
/// busy). The single production delivery path for the idle send:pty verb and the
/// fakerepl R4 gate row.
///
/// Sequence:
///  1. `send_text(message)` — text ALONE (no CR).
///  2. `sleep(TWO_WRITE_SETTLE_MS)` — let the text burst close.
///  3. `send_cr()` — a SEPARATE "\r" keystroke (a paste burst does NOT collapse
///     this; it submits the composed text, mimicking a human's Enter).
///  4. `verify_accepted_then_cr` over a content-verified `send_cr`: confirm the
///     session went busy; if not, emit AT MOST ONE remediation CR, and ONLY while
///     the composer provably still holds OUR text (never blind).
pub fn deliver_idle_two_write(
    deps: &dyn IdleDeliverDeps,
    message: &str,
    opts: SubmitOptions,
) -> SubmitOutcome {
    deliver_idle_two_write_with(deps, message, opts, ChunkSendOptions::default())
}

/// As [`deliver_idle_two_write`], but with an explicit [`ChunkSendOptions`] so the
/// chunk_bytes/settle_ms seams are injectable (defaults 1024B/150ms — fakerepl rows
/// pass a tiny settle and/or a forced chunk size for speed / overflow modelling).
///
/// The text phase is now CHUNKED (ADR 0009 mode (a)): the text is split into
/// ≤chunk_bytes code-point-safe chunks, each sent as its OWN `send_text` write with a
/// `settle_ms` inter-chunk sleep, so a large write does NOT overflow the tty queue
/// and get dropped wholesale. The two-write \r + content-verified remediation
/// discipline (fix (b)) runs UNCHANGED on top. For a message ≤chunk_bytes this is a
/// single `send_text` with no inter-chunk sleep — byte-identical to the prior single
/// text write.
pub fn deliver_idle_two_write_with(
    deps: &dyn IdleDeliverDeps,
    message: &str,
    opts: SubmitOptions,
    chunk_opts: ChunkSendOptions,
) -> SubmitOutcome {
    // --- Chunked two-write delivery (chunked text, settle, separate CR) -------
    // Mode (a): the text phase is chunked so a ≥~4KB write does not overflow the
    // tty queue and drop wholesale. send_text_chunked drives the trait's own
    // send_text/sleep, so EVERY IdleDeliverDeps impl (real + fake) is chunked.
    send_text_chunked(
        &mut |chunk| deps.send_text(chunk),
        &mut |ms| deps.sleep(ms),
        message,
        chunk_opts,
    );
    deps.sleep(TWO_WRITE_SETTLE_MS);
    deps.send_cr();

    // --- Acceptance-keyed, CONTENT-VERIFIED verify-then-CR --------------------
    let verified = ContentVerifiedSubmit::new(deps, message);
    verify_accepted_then_cr(&verified, opts)
}

// ===========================================================================
// deliver_prompt — bounded-retry wrapper (a4-spec §2.2).
//
// Port of qa/hardening@3dd9f1e:src/commands/lifecycle.ts:265-345 (sendCR binding
// :265-271, makeRealSubmitDeps :279-290, deliverPrompt :302-345), adapted to the
// THREE-WAY DeliverOutcome the went-busy exit contract requires (spec-red-team
// R1): TS collapses the find-pid-file miss and the never-went-busy stall into one
// `false` (lifecycle.ts:309-311); the exit contract must NOT.
// ===========================================================================

/// Three-way outcome of [`deliver_prompt`] (a4-spec §2.2, spec-red-team R1).
///
/// TS returns `accepted: bool` (lifecycle.ts:303,345). The Rust exit contract
/// (ADR 0008, M2) needs to distinguish a STALL (PID file readable, status simply
/// never reached busy) from an INFRA failure (PID file vanished post-boot). A
/// `PidFileMissing` routed to the "stalled → exit 10" bucket would lie to an
/// external `bond spawn`, so it stays distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// Session went busy — the turn started.
    Accepted,
    /// PID file READABLE, but status never reached busy after full bounded
    /// remediation (TS `false`, lifecycle.ts:311/345).
    Stalled,
    /// `find_pid_file` returned None — the registry row vanished post-boot. An
    /// infra failure, NOT a stall (R1). TS also returns `false` here
    /// (lifecycle.ts:309-311), collapsing it; we keep it distinct.
    PidFileMissing,
}

/// The effects [`deliver_prompt`] needs, injected so it is testable without a
/// live mux / real PID files (mirrors the TS `makeRealSubmitDeps` split:
/// lifecycle.ts:279-290 — pure logic in submit.ts, this adapter binds it to a
/// live session). The real binding is [`RealDeliverDeps`].
///
/// R4 (ADR 0009, LEAD EXTENSION of the orc-2 ruling): `send_message` now delivers
/// the R4 TWO-WRITE shape (text, settle, separate CR) — the create path's priming
/// prompts (external `bond spawn`) are the LIKELIEST ≥4KB case and share the exact
/// single-write loss mechanism the ruling names on the idle send:pty path. Each
/// remediation round's CR is CONTENT-VERIFIED via the new `read_screen` hook (the
/// per-round [`SubmitDeps`] from `submit_deps` reads the screen and CRs only while
/// the composer provably still holds the message). Bounded rounds + exit contract
/// unchanged. The ruling names the idle send:pty path; the lead extends to
/// deliver_prompt on the SAME-mechanism grounds, flagged for ratification at the
/// merge ruling.
pub trait DeliverDeps {
    /// Deliver `message` with the R4 two-write shape (text alone, ~200ms settle,
    /// "\r" alone) — NOT a single `message + "\r"` write (ADR 0009; the old TS
    /// shape, lifecycle.ts:307-308, is paste-absorbed at ≥~4KB).
    fn send_message(&self, message: &str);
    /// The session's screen (`zmx history`) for the content-verified remediation
    /// CR (ADR 0009 LEAD EXTENSION). A read error yields an empty screen → no CR.
    fn read_screen(&self) -> String;
    /// Locate the session's PID file (TS `findPidFile(name, 5000)`,
    /// lifecycle.ts:311). `None` → the row never appeared / vanished.
    fn find_pid_file(&self) -> Option<PathBuf>;
    /// Build the per-session [`SubmitDeps`] bound to `pid_file` AND to `message`,
    /// whose `send_cr` is CONTENT-VERIFIED against [`read_screen`] (TS
    /// `makeRealSubmitDeps(zmxName, pidFile)`, lifecycle.ts:312, now content-keyed
    /// per ADR 0009). The bounded-retry loop drives this unchanged.
    fn submit_deps(&self, pid_file: PathBuf, message: &str) -> Box<dyn SubmitDeps + '_>;
}

/// Deliver a prompt to a session that is already idle, then apply the
/// acceptance-keyed verify-then-CR submit discipline, made LOAD-ROBUST with
/// bounded retries. Port of `deliverPrompt`
/// (qa/hardening@3dd9f1e:src/commands/lifecycle.ts:302-345).
///
/// `timeout_s` defaults to 15 (TS `timeoutSec = 15`, lifecycle.ts:303); `qd new
/// -p` calls it with NO override (lifecycle.ts:921) → per-round post_cr = 12.5s
/// (N9 — do NOT wire send:pty's 120s here).
///
/// ---------------------------------------------------------------------------
/// VERBATIM war-story (qa/hardening@3dd9f1e:src/commands/lifecycle.ts:320-338):
///
/// Acceptance-keyed verify-then-CR, made LOAD-ROBUST with BOUNDED retries (P3 fix
/// round; red-team r1+r2). The first attempt is the original single settle + one
/// CR. Under representative multi-session load even that single CR intermittently
/// fails to submit a freshly-booted session (the create-turn paste-race: FINDING-E1
/// class; r2 reproduced a boot "did not go busy" at moderate load when several
/// helpers boot back-to-back). So escalate with up to `maxRounds` further rounds,
/// each = busy-guard re-check (settleMs:0 → immediate re-read, never CRs a session
/// that just went busy) + EXACTLY ONE more CR + a GENEROUS re-poll so a slow submit
/// registers before another CR. Bounded (never blanket-CR), keyed on ACCEPTANCE,
/// preserves the no-spurious-empty-turn invariant, and still FAILS CLOSED (returns
/// false → the retired `spawn` verb's boot-stuck exit-1) if acceptance never happens. The
/// verifyAcceptedThenCR core (and its unit tests) is unchanged; this only wraps it.
/// ---------------------------------------------------------------------------
pub fn deliver_prompt(deps: &dyn DeliverDeps, message: &str, timeout_s: u64) -> DeliverOutcome {
    // Deliver with the R4 two-write shape (text, settle, separate CR) — NOT a
    // single `message + "\r"` write, which is paste-absorbed at ≥~4KB (ADR 0009
    // LEAD EXTENSION; the priming-prompt ≥4KB case). The impl owns the two writes.
    deps.send_message(message);

    // Find the PID file (should already exist since we just waited for ready).
    // R1: None is PidFileMissing (infra), NOT a stall.
    let Some(pid_file) = deps.find_pid_file() else {
        return DeliverOutcome::PidFileMissing;
    };
    // The per-round SubmitDeps is CONTENT-VERIFIED against the screen (ADR 0009):
    // each round's remediation CR fires ONLY while the composer still holds
    // `message`. Bounded rounds + acceptance-keying unchanged.
    let submit = deps.submit_deps(pid_file, message);

    let settle_ms = 2500; // first-attempt settle (r1 nit F7)
    let per_round_ms = (timeout_s * 1000).saturating_sub(2500).max(8000); // generous post-CR re-poll per round
    let max_rounds = 3; // bounded extra remediation rounds

    let mut outcome = verify_accepted_then_cr(
        submit.as_ref(),
        SubmitOptions {
            settle_ms,
            post_cr_ms: per_round_ms,
            poll_ms: SubmitOptions::default().poll_ms,
        },
    );
    let mut round = 0;
    while !outcome.accepted && round < max_rounds {
        outcome = verify_accepted_then_cr(
            submit.as_ref(),
            SubmitOptions {
                settle_ms: 0,
                post_cr_ms: per_round_ms,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        round += 1;
    }

    if outcome.accepted {
        DeliverOutcome::Accepted
    } else {
        DeliverOutcome::Stalled
    }
}

/// Default delivery timeout in seconds (TS `timeoutSec = 15`,
/// qa/hardening@3dd9f1e:src/commands/lifecycle.ts:303).
pub const DELIVER_TIMEOUT_S: u64 = 15;

// ===========================================================================
// Real bindings (the M2 bin layer wires these; they compile here per a4-spec
// §2.2 "the real binding compiles but its bin wiring is M2's, not yours"). The
// caller resolves the canonical socket dir via `zmx_dir::resolve_zmx_dir(env)`
// and passes it as `dir` (the create path: the session was just born there).
// ===========================================================================

/// The real (Mux/fs/clock-backed) [`SubmitDeps`], binding the pure discipline to
/// a live session's PID file + zmx name. Port of `makeRealSubmitDeps`
/// (qa/hardening@3dd9f1e:src/commands/lifecycle.ts:279-290).
///
/// `dir` is the session's ACTUAL socket dir so the CR lands in the same session
/// the original message did, even when the caller's ZMX_DIR points elsewhere
/// (Bug D; lifecycle.ts:257-263 `sendCR` comment).
pub struct RealSubmitDeps<'a> {
    pub mux: &'a dyn Mux,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
    pub zmx_name: String,
    pub pid_file: PathBuf,
    pub dir: PathBuf,
}

impl SubmitDeps for RealSubmitDeps<'_> {
    fn read_status(&self) -> Option<String> {
        read_pid_status(&self.pid_file)
    }
    fn send_cr(&self) {
        // VERBATIM (qa/hardening@3dd9f1e:src/commands/lifecycle.ts:257-263):
        // Send exactly one carriage return to a zmx session (the verify-then-CR
        // remediation). Targets `dir` — the session's ACTUAL socket dir — so the
        // CR lands in the same session the original message did, even when the
        // caller's ZMX_DIR points elsewhere (Bug D). Defaults to the canonical
        // dir for the create path, where the session was just born in this
        // process's canonical dir.
        let _ = self.mux.send(&self.dir, &self.zmx_name, "\r");
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// Convenience: build a [`RealSubmitDeps`] from live effects and run
/// [`verify_accepted_then_cr`] with the default [`SubmitOptions`]. This is the
/// send:pty IDLE-path remediation (a4-spec §3.1 step 6 / send.ts:185-196):
/// `makeRealSubmitDeps(zmxName, pidFile, opDir)` → `verifyAcceptedThenCR`. Pinned
/// to `dir` (the session's ACTUAL socket dir, Bug D).
#[allow(clippy::too_many_arguments)]
pub fn verify_then_cr_real(
    mux: &dyn Mux,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    zmx_name: &str,
    dir: &std::path::Path,
    pid_file: &std::path::Path,
) -> SubmitOutcome {
    let deps = RealSubmitDeps {
        mux,
        clock,
        sleeper,
        zmx_name: zmx_name.to_string(),
        pid_file: pid_file.to_path_buf(),
        dir: dir.to_path_buf(),
    };
    verify_accepted_then_cr(&deps, SubmitOptions::default())
}

/// The real (Mux/fs/clock-backed) [`IdleDeliverDeps`] for the R4 two-write idle
/// send:pty path (ADR 0009). `send_text`/`send_cr` are raw `zmx send` pinned to the
/// session's ACTUAL socket `dir` (Bug D — both writes AND the screen read hit the
/// SAME session); `read_screen` is `zmx history` pinned to the same dir; status is
/// the pid-file read.
pub struct RealIdleDeliverDeps<'a> {
    pub mux: &'a dyn Mux,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
    pub zmx_name: String,
    pub pid_file: PathBuf,
    pub dir: PathBuf,
}

impl IdleDeliverDeps for RealIdleDeliverDeps<'_> {
    fn send_text(&self, text: &str) {
        // First write: the text ALONE, no CR (R4 two-write; send.ts:163).
        let _ = self.mux.send(&self.dir, &self.zmx_name, text);
    }
    fn send_cr(&self) {
        // The SEPARATE "\r" keystroke (second write + content-verified remediation
        // CR), pinned to `dir` (Bug D), so it lands in the same session the text
        // did (send.ts:165,177).
        let _ = self.mux.send(&self.dir, &self.zmx_name, "\r");
    }
    fn read_screen(&self) -> String {
        // `zmx history` pinned to the SAME dir (Bug D). A read error yields an
        // empty screen → composer_holds_message is false → no blind CR (fail-safe;
        // send.ts readScreen).
        self.mux
            .history(&self.dir, &self.zmx_name)
            .unwrap_or_default()
    }
    fn read_status(&self) -> Option<String> {
        read_pid_status(&self.pid_file)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// Convenience: build a [`RealIdleDeliverDeps`] from live effects and run
/// [`deliver_idle_two_write`] with the default [`SubmitOptions`] — the R4 idle
/// send:pty delivery (ADR 0009). Replaces the single `message + "\r"` write +
/// [`verify_then_cr_real`] (send.ts:204-209). Pinned to `dir` (Bug D).
#[allow(clippy::too_many_arguments)]
pub fn deliver_idle_two_write_real(
    mux: &dyn Mux,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    zmx_name: &str,
    dir: &std::path::Path,
    pid_file: &std::path::Path,
    message: &str,
) -> SubmitOutcome {
    let deps = RealIdleDeliverDeps {
        mux,
        clock,
        sleeper,
        zmx_name: zmx_name.to_string(),
        pid_file: pid_file.to_path_buf(),
        dir: dir.to_path_buf(),
    };
    deliver_idle_two_write(&deps, message, SubmitOptions::default())
}

/// The real (Mux/fs-backed) [`DeliverDeps`], pinned to the canonical dir for the
/// create path (the session was just born there). Port of the `deliverPrompt`
/// effect bindings (qa/hardening@3dd9f1e:src/commands/lifecycle.ts:302-312). The
/// bin wiring (M2) constructs this; it compiles here.
pub struct RealDeliverDeps<'a> {
    pub mux: &'a dyn Mux,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
    /// The zmx session name to send to.
    pub zmx_name: String,
    /// The session NAME the registry row is keyed on (find_pid_file match).
    pub session_name: String,
    /// Injected registry dir (L9a: never the real home).
    pub sessions_dir: PathBuf,
    /// The canonical socket dir (caller resolves via `resolve_zmx_dir(env)`).
    pub dir: PathBuf,
}

impl DeliverDeps for RealDeliverDeps<'_> {
    fn send_message(&self, message: &str) {
        // CHUNKED TWO-WRITE delivery pinned to the canonical dir (create path:
        // session just born there; ADR 0009). The text is split into ≤1024B
        // code-point-safe chunks (mode (a): a single large write overflows the tty
        // queue and drops wholesale — the priming prompt is the likeliest ≥~4KB
        // case), then ~200ms settle + a SEPARATE "\r" (mode (b): a single
        // `message + "\r"` is paste-absorbed). Replaces lifecycle.ts:307-308.
        send_text_chunked(
            &mut |chunk| {
                let _ = self.mux.send(&self.dir, &self.zmx_name, chunk);
            },
            &mut |ms| self.sleeper.sleep_ms(ms),
            message,
            ChunkSendOptions::default(),
        );
        self.sleeper.sleep_ms(TWO_WRITE_SETTLE_MS);
        let _ = self.mux.send(&self.dir, &self.zmx_name, "\r");
    }
    fn read_screen(&self) -> String {
        // `zmx history` pinned to the canonical dir (Bug D); error → empty → no CR.
        self.mux
            .history(&self.dir, &self.zmx_name)
            .unwrap_or_default()
    }
    fn find_pid_file(&self) -> Option<PathBuf> {
        // TS `findPidFile(sessionName, 5000)` (lifecycle.ts:311). The poll/clock
        // seam is the existing boot.rs primitive; 250ms poll is the boot default.
        find_pid_file(
            &self.sessions_dir,
            &self.session_name,
            5000,
            250,
            self.clock,
            self.sleeper,
        )
    }
    fn submit_deps(&self, pid_file: PathBuf, message: &str) -> Box<dyn SubmitDeps + '_> {
        // CONTENT-VERIFIED per-round SubmitDeps (ADR 0009 LEAD EXTENSION): the
        // remediation CR fires only while the composer still holds `message`.
        Box::new(ContentVerifiedSubmit::new(
            RealIdleDeliverDeps {
                mux: self.mux,
                clock: self.clock,
                sleeper: self.sleeper,
                zmx_name: self.zmx_name.clone(),
                pid_file,
                dir: self.dir.clone(),
            },
            message,
        ))
    }
}

// ===========================================================================
// W8 — verify-after-submit (silent mid-truncation closure; A4 R1 / D16 flip).
//
// SANCTIONED design: exec/a7-evidence/a4-r1-truncation-closure-proposal.md (M11),
// Pete ADD-15 W8, wart-wave-spec §4 (post red-team R1+R8). After a CHUNKED
// delivery reports went-busy, the writer cannot tell whether every chunk reached
// the composer: under a sustained PTY-reader stall mid-delivery the tty queue can
// saturate and mid-payload bytes drop SILENTLY (submit keys on went-busy, not
// payload arrival). The closure reads the session JSONL back and verifies the
// submitted user record carries the WHOLE payload.
//
// Policy guardrail (the design enemy is the FALSE FAIL): loud-fail ONLY on
// POSITIVE truncation evidence (a shorter record that shares the message's leading
// bytes). Everything else — no record, unattributable record, transient read
// failure — DEGRADES (the caller warns; success path unchanged). The pure helper
// here returns the outcome; the bin layer (M5) maps it to stderr/exit.
//
// This helper is PURE over injected [`VerifyDeps`] so it is unit-testable and
// drivable by the fakerepl gate without a live fs/clock. The real wiring (M5)
// binds `read_user_texts` to `parse_jsonl_slice` + `user_record_text` over the
// JSONL slice past the pre-delivery offset.
// ===========================================================================

/// Bounded verify budget in seconds (wart-wave-spec §4 step 2: poll up to 10s).
pub const VERIFY_TIMEOUT_S: u64 = 10;

/// Verify poll interval in ms (the 500ms JSONL poll, same cadence as `--wait`).
pub const VERIFY_POLL_MS: u64 = 500;

/// Bounded verify budget for the BUSY-QUEUED deferred verify (D2 §7-B RE-SCAN,
/// binding constraint 2). SEPARATE from the shared 10s [`VERIFY_TIMEOUT_S`] — a
/// busy-queued turn only lands AFTER the prior in-flight turn completes (~15-20s,
/// cert-measured), so a 10s budget would expire mid-prior-turn → a false PENDING.
/// ~120s matches the `--wait` anchor horizon. It stays BOUNDED: a genuinely lost
/// send degrades to PENDING (no emit), NEVER a hang. The idle / `--wait` / >1-chunk
/// paths keep their 10s untouched (constraint 4).
pub const BUSY_QUEUED_VERIFY_TIMEOUT_S: u64 = 120;

/// How many leading bytes of a record must match the message prefix for the
/// record to count as the same-turn truncation signature (covers both prefix and
/// mid-loss shapes — a mid-loss record `first-1KB ++ last-1KB` still shares the
/// message's leading bytes). `min(64, recorded.len())` bytes are compared.
const TRUNC_SIGNATURE_PREFIX: usize = 64;

/// Predicate: does `message` need post-delivery verification? TRUE iff the
/// PRODUCTION splitter ([`chunk_text`]) yields more than one chunk — never a
/// byte-count re-derivation (wart-wave-spec red-team R3: ">1024B" and ">1 chunk"
/// are NOT equivalent at the seam — a 1023-ASCII-byte message plus one 3-byte
/// code point is 1026 bytes but still ONE chunk, since a chunk never splits a code
/// point and the whole thing fits the 1024B budget only if... it does not — the
/// CHUNK COUNT, from the real splitter, is the single source of truth). A
/// single-chunk submit keeps today's behavior byte-for-byte (scope guard).
pub fn payload_needs_verify(message: &str) -> bool {
    chunk_text(message, CHUNK_BYTES).len() > 1
}

/// The fs read both bin wirings bind [`VerifyDeps::read_user_texts`] to: read the
/// transcript, slice past `offset`, parse, collect user-record texts in file
/// order. Error semantics match the M5 wiring contract:
///   - file ABSENT/unreadable → `Err` (resolution failed this poll; re-polled —
///     on the `qd new -p` path the transcript may not exist yet);
///   - file SHRANK below `offset`, or `offset` off a char boundary → `Err`
///     (integrity; re-reading from byte 0 could match an OLD record — the same
///     wrong-anchor class the --wait loop fails loud on);
///   - otherwise `Ok(texts)` (possibly empty).
pub fn read_user_texts_past_offset(
    path: &std::path::Path,
    offset: u64,
) -> Result<Vec<String>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("transcript unreadable ({e})"))?;
    let off = offset as usize;
    if content.len() < off {
        return Err(format!(
            "transcript shrank below the pre-delivery offset ({} < {off} bytes)",
            content.len()
        ));
    }
    let Some(slice) = content.get(off..) else {
        return Err(format!("pre-delivery offset {off} is not a char boundary"));
    };
    Ok(crate::sendpty::parse_jsonl_slice(slice)
        .iter()
        .filter_map(|p| {
            let rec: crate::sendpty::JsonlRecord =
                serde_json::from_value(p.value.clone()).unwrap_or_default();
            crate::sendpty::user_record_text(&rec)
        })
        .collect())
}

/// The byte offset of the FIRST user-text record in a transcript — the NIT-2
/// HIGH-WATER floor for a freshly-spawned child (RESPEC-DELTA §3.2). Iterates the
/// JSONL line-by-line tracking the cumulative byte offset and returns the offset
/// of the first line that parses as a user record carrying text (the SAME
/// [`crate::sendpty::user_record_text`] filter the verify uses, so the leading
/// non-user init records a fresh transcript opens with — `custom-title` /
/// `agent-name` / `mode` / `permission-mode` / `file-history-snapshot` — are
/// skipped). `None` if the file is unreadable or no user-text record has landed
/// yet (the caller keeps polling).
///
/// WHY this is the right floor (the binding NIT-2 anti-wrong-fire): the deferred
/// path is reached ONLY when the transcript did NOT exist at send-time
/// ⇒ a genuinely fresh session ⇒ NO prior user records. So the first user-text
/// record is THIS turn's own record. Anchoring the content-sha scan here (never
/// byte 0 — it is past the ~640B of init records) means an exact-text re-scan can
/// only match THIS turn, never an EARLIER identical body (which cannot exist).
pub fn first_user_text_offset(path: &std::path::Path) -> Option<u64> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut offset: u64 = 0;
    // `split_inclusive` keeps the trailing '\n' on each piece, so summing
    // `piece.len()` walks the exact byte offsets; a line boundary is always a
    // char boundary, so the returned offset slices cleanly in the reader.
    for piece in content.split_inclusive('\n') {
        let trimmed = piece.trim_end_matches(['\n', '\r']);
        if !trimmed.is_empty() {
            if let Ok(rec) = serde_json::from_str::<crate::sendpty::JsonlRecord>(trimmed) {
                if crate::sendpty::user_record_text(&rec).is_some() {
                    return Some(offset);
                }
            }
        }
        offset += piece.len() as u64;
    }
    None
}

/// Outcome of [`verify_chunked_payload`] (wart-wave-spec §4 step 2). The bin layer
/// maps each to its stderr/exit: `Verified` → unchanged success; `Truncated` →
/// loud-fail exit 1 (the existing delivery-failure class, fired AFTER went-busy);
/// `Unattributable`/`NoRecord`/`SourceUnavailable` → ONE stderr warn, success
/// path otherwise unchanged (degrade, never false-fail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadVerifyOutcome {
    /// A user record past the offset matched the message byte-exact — the whole
    /// payload arrived.
    Verified,
    /// At budget end, no exact match but a record SHORTER than the message that
    /// shares its leading bytes (the same-turn truncation signature). `expected`
    /// = the sent message length; `recorded` = the truncated record length.
    Truncated { expected: usize, recorded: usize },
    /// At budget end, records exist past the offset but none matched and none
    /// carries the truncation signature (e.g. a foreign / multi-block record).
    /// Degrade-warn, never loud-fail.
    Unattributable,
    /// At budget end, at least one read succeeded but ZERO records were ever seen
    /// past the offset (the turn never landed a user record in the budget).
    NoRecord,
    /// EVERY read attempt errored (transcript/offset resolution never succeeded in
    /// the budget). Carries the last failure reason.
    SourceUnavailable(String),
}

/// Effects [`verify_chunked_payload`] needs, injected so the policy is
/// unit-testable without a live fs/clock and drivable by the fakerepl gate.
pub trait VerifyDeps {
    /// Read the user-record texts past the pre-delivery offset, in FILE ORDER.
    ///
    /// `Ok(texts)` = a successful read (possibly empty if no user record landed
    /// yet). The real wiring slices the JSONL past the offset, runs
    /// [`parse_jsonl_slice`](crate::sendpty::parse_jsonl_slice) +
    /// [`user_record_text`](crate::sendpty::user_record_text), and collects the
    /// `Some` texts.
    ///
    /// `Err(reason)` = transcript/offset resolution failed THIS poll (the file
    /// shrank past the offset, the session_id is not yet resolvable on the
    /// `qd new -p` path, etc.). NOT terminal — the loop re-polls; only if EVERY
    /// read errors through the budget does the outcome become `SourceUnavailable`.
    fn read_user_texts(&self) -> Result<Vec<String>, String>;
    /// Sleep `ms` (the 500ms poll; seamed so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic-ish clock in ms (the budget deadline).
    fn now_ms(&self) -> i64;
}

/// Does `record` carry the same-turn truncation signature against `message`?
/// True iff it is non-empty, strictly SHORTER than `message`, and its first
/// `min(TRUNC_SIGNATURE_PREFIX, record.len())` bytes equal the same-length prefix
/// of `message`. (A record EQUAL in length is handled by the exact-match path; a
/// LONGER record is never our truncated payload.)
fn carries_truncation_signature(record: &str, message: &str) -> bool {
    let rec = record.as_bytes();
    let msg = message.as_bytes();
    if rec.is_empty() || rec.len() >= msg.len() {
        return false;
    }
    let n = rec.len().min(TRUNC_SIGNATURE_PREFIX);
    rec[..n] == msg[..n]
}

/// Bounded post-delivery verification of a CHUNKED payload (wart-wave-spec §4
/// step 2, post red-team R1+R8). Polls `read_user_texts` every `poll_ms` for up to
/// `timeout_s`:
///
/// - ANY record text == `message` (byte-exact) → [`PayloadVerifyOutcome::Verified`]
///   immediately (incl. on a later poll — a slow-resolving transcript still wins).
/// - At budget end (no exact match), evaluate the LAST successful read's records:
///   * a truncation-signature candidate (shorter + shared leading bytes) →
///     [`PayloadVerifyOutcome::Truncated`] (the LONGEST candidate if several);
///   * records exist but none match / none carries the signature →
///     [`PayloadVerifyOutcome::Unattributable`];
///   * zero records ever seen but at least one read succeeded →
///     [`PayloadVerifyOutcome::NoRecord`];
///   * EVERY read errored → [`PayloadVerifyOutcome::SourceUnavailable`].
pub fn verify_chunked_payload(
    deps: &dyn VerifyDeps,
    message: &str,
    timeout_s: u64,
    poll_ms: u64,
) -> PayloadVerifyOutcome {
    let deadline = deps.now_ms() + (timeout_s * 1000) as i64;
    // The LAST successful read's records (None until a read succeeds); and the
    // last failure reason (for SourceUnavailable when no read ever succeeds).
    let mut last_records: Option<Vec<String>> = None;
    let mut last_reason: Option<String> = None;

    // Poll: read immediately, then re-poll until the deadline (so a 0ms budget
    // still does one read — mirrors wait_for_busy).
    loop {
        match deps.read_user_texts() {
            Ok(texts) => {
                if texts.iter().any(|t| t == message) {
                    return PayloadVerifyOutcome::Verified;
                }
                last_records = Some(texts);
            }
            Err(reason) => {
                last_reason = Some(reason);
            }
        }
        if deps.now_ms() >= deadline {
            break;
        }
        let remaining = (deadline - deps.now_ms()).max(0) as u64;
        deps.sleep(poll_ms.min(remaining));
    }

    // Budget exhausted, no exact match. Evaluate the last successful read.
    let Some(records) = last_records else {
        // No read ever succeeded → resolution never worked in the budget.
        return PayloadVerifyOutcome::SourceUnavailable(
            last_reason.unwrap_or_else(|| "transcript unreadable".to_string()),
        );
    };

    if records.is_empty() {
        // Reads succeeded but the turn never landed a user record in the budget.
        return PayloadVerifyOutcome::NoRecord;
    }

    // POSITIVE truncation evidence: the LONGEST record carrying the same-turn
    // signature (shorter + shared leading bytes). Longest = the most complete
    // truncated candidate (R8: pick the best evidence, not the first).
    let truncated = records
        .iter()
        .filter(|r| carries_truncation_signature(r, message))
        .map(|r| r.len())
        .max();
    if let Some(recorded) = truncated {
        return PayloadVerifyOutcome::Truncated {
            expected: message.len(),
            recorded,
        };
    }

    // Records exist but none matched / none carries the signature — foreign or
    // multi-block. Degrade-warn, NEVER loud-fail (R8).
    PayloadVerifyOutcome::Unattributable
}

/// Effects the DISPATCH-PTY deferred fresh-child verify needs (RESPEC-DELTA §3.2),
/// injected so the policy is unit-testable without a live fs/clock.
pub trait DeferredVerifyDeps {
    /// Resolve the recipient transcript path — `None` until a freshly-spawned
    /// child's transcript materializes. The real wiring polls the SAME call the
    /// relay observer uses (`relay_server/mod.rs:929`):
    /// `find_jsonl_path(projects_dir, session_id, None)`.
    fn resolve_path(&self) -> Option<PathBuf>;
    /// The NIT-2 HIGH-WATER floor: the byte offset the content scan anchors PAST.
    /// Fresh-child wiring returns the first user-text record offset in `path`
    /// ([`first_user_text_offset`]), `None` until one lands. The §7-B busy-queued
    /// wiring instead returns the CAPTURED PRE-SEND OFFSET (the analogue floor for
    /// an existing session — past the prior in-flight turn's user record), so the
    /// `path` arg is unused there. Either way: never byte 0, so an exact-text
    /// re-scan can only match THIS turn, never an EARLIER identical body.
    fn first_user_offset(&self, path: &std::path::Path) -> Option<u64>;
    /// User-record texts at/after `offset`, in file order (the content match —
    /// [`read_user_texts_past_offset`]).
    fn read_user_texts(&self, path: &std::path::Path, offset: u64) -> Result<Vec<String>, String>;
    /// Sleep `ms` (the poll cadence; seamed so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic-ish clock in ms (the budget deadline).
    fn now_ms(&self) -> i64;
}

/// Bounded DEFERRED verification for a no-wait pty send to a FRESHLY-spawned child
/// — the S1 root-cause fix (RESPEC-DELTA §3). The transcript is unresolvable at
/// send-time for a just-spawned child, so the W8 verify's `None` arm could only
/// warn and the priming chain stalled (no `message-seen` ⇒ bond's on-received gate
/// never advanced ⇒ links 2..n never fired). This polls the transcript into
/// existence (modeled on the relay observer's per-poll `find_jsonl_path`), anchors
/// the content scan at the NIT-2 high-water floor (the first user-text record's
/// offset — never byte 0), and confirms the sent content landed.
///
/// Returns [`PayloadVerifyOutcome::Verified`] iff EXACTLY ONE user record at/after
/// the high-water floor matches `message` byte-exact. This strengthens
/// [`verify_chunked_payload`]'s any-match into a UNIQUENESS check — the binding
/// NIT-2 "PENDING-on-ambiguity": if ≥2 identical bodies sit past the floor (which
/// cannot arise for a genuinely fresh child, but would for a transiently-
/// unresolvable EXISTING session), it REFUSES rather than anchor on the wrong one.
/// Any non-`Verified` outcome ([`PayloadVerifyOutcome::Unattributable`] = ambiguous,
/// `NoRecord` = floor seen but the turn never matched, `SourceUnavailable` = the
/// transcript never resolved) is the caller's signal to degrade to a VISIBLE
/// PENDING stall — never a wrong-fire.
pub fn deferred_verify_fresh_child(
    deps: &dyn DeferredVerifyDeps,
    message: &str,
    timeout_s: u64,
    poll_ms: u64,
) -> PayloadVerifyOutcome {
    deferred_verify_floor_core(
        deps,
        message,
        timeout_s,
        poll_ms,
        "fresh-child transcript never resolved within the deferred window",
    )
}

/// FLOOR-PARAMETERIZED CORE of the deferred uniqueness verify (D2 §7-B RE-SCAN,
/// §4.2). The bounded, high-water-anchored, uniqueness-checked poll-loop shared by
/// BOTH deferred callers — [`deferred_verify_fresh_child`] (the S1 fresh-child fix)
/// and [`deferred_verify_busy_queued`] (the §7-B busy-queued fix). The ONLY thing
/// that differs between them is the FLOOR (and where it comes from), and the floor
/// is supplied entirely by the injected `deps` ([`DeferredVerifyDeps::first_user_offset`]):
///   - fresh-child: the first user-text record offset, discovered per-poll once the
///     freshly-spawned transcript materializes (`first_user_text_offset`);
///   - busy-queued: the CAPTURED PRE-SEND OFFSET (snapshot before typing, past the
///     prior in-flight turn's user record), known up front for an existing session.
/// Each poll: resolve the (path, floor); read the user texts at/after the floor;
/// apply the NIT-2 uniqueness rule — EXACTLY ONE byte-exact match → `Verified`;
/// `≥2` identical bodies past the floor → ambiguous (→ `Unattributable`, refuse,
/// never anchor on the wrong one); `0` → keep polling. Bounded by `timeout_s`; at
/// budget end it degrades (`Unattributable` / `NoRecord` / `SourceUnavailable`,
/// using `unresolved_reason` only when no read ever succeeded) — NEVER a false-fire.
fn deferred_verify_floor_core(
    deps: &dyn DeferredVerifyDeps,
    message: &str,
    timeout_s: u64,
    poll_ms: u64,
    unresolved_reason: &str,
) -> PayloadVerifyOutcome {
    let deadline = deps.now_ms() + (timeout_s * 1000) as i64;
    let mut saw_floor = false;
    let mut ambiguous = false;
    let mut last_reason: Option<String> = None;
    loop {
        if let Some(path) = deps.resolve_path() {
            if let Some(floor) = deps.first_user_offset(&path) {
                saw_floor = true;
                match deps.read_user_texts(&path, floor) {
                    Ok(texts) => {
                        let matches = texts.iter().filter(|t| t.as_str() == message).count();
                        if matches == 1 {
                            return PayloadVerifyOutcome::Verified;
                        }
                        if matches >= 2 {
                            // NIT-2 binding fallback: an exact-text re-scan that
                            // could anchor on an earlier identical body MUST
                            // refuse → PENDING, never fire.
                            ambiguous = true;
                        }
                        // matches == 0 → this turn has not landed past the floor
                        // yet; keep polling.
                    }
                    Err(reason) => last_reason = Some(reason),
                }
            }
        }
        if deps.now_ms() >= deadline {
            break;
        }
        let remaining = (deadline - deps.now_ms()).max(0) as u64;
        deps.sleep(poll_ms.min(remaining).max(1));
    }
    if ambiguous {
        PayloadVerifyOutcome::Unattributable
    } else if saw_floor {
        // The transcript + a user record resolved, but our exact content never
        // appeared past the floor in the window → not-yet-seen → PENDING.
        PayloadVerifyOutcome::NoRecord
    } else {
        PayloadVerifyOutcome::SourceUnavailable(
            last_reason.unwrap_or_else(|| unresolved_reason.to_string()),
        )
    }
}

/// Bounded DEFERRED verification for a BUSY-QUEUED no-wait pty send — the D2 §7-B
/// RE-SCAN fix (PTY-FIX-DESIGN.md §4). When bond fires a chain link at a child that
/// is still mid-turn, `send:pty` classifies it `SendQueue` and (pre-fix) returned
/// without any verify → the busy-queued path was STRUCTURALLY incapable of emitting
/// `message-seen` → a create-head `--via pty` chain of ≥3 links stalled at link-2
/// (bond's on-received never opened link-3). This extends the proven deferred verify
/// to that path: it polls the (already-resolvable) transcript at/after the CAPTURED
/// PRE-SEND OFFSET — the high-water floor that sits PAST the prior in-flight turn's
/// user record (constraint 1) — until the queued turn's record genuinely lands.
///
/// SEMANTIC MUST (PTY-FIX-DESIGN.md §3): on a unique match the CALLER emits the SAME
/// `message-seen` the idle path emits (→ bond `Fired{Seen}`, trips on-received) —
/// never `turn-anchored`. Returns [`PayloadVerifyOutcome::Verified`] iff EXACTLY ONE
/// user record at/after the floor matches `message` byte-exact (the NIT-2 uniqueness
/// guarantee, constraint 3). The high-water floor means the prior in-flight turn's
/// record — even an IDENTICAL body — is excluded from the scan, so a re-scan can
/// only anchor on THIS turn, never an earlier identical one. Any non-`Verified`
/// outcome (≥2 identical bodies → `Unattributable`; floor seen but no match →
/// `NoRecord`; transcript unreadable → `SourceUnavailable`) is the caller's signal
/// to degrade to a VISIBLE PENDING stall — never a wrong-fire.
///
/// Identical body to [`deferred_verify_fresh_child`] (both delegate to
/// [`deferred_verify_floor_core`]); they differ ONLY in the floor the injected
/// `deps` supplies. Drive it with [`BUSY_QUEUED_VERIFY_TIMEOUT_S`] (constraint 2),
/// NOT the shared 10s [`VERIFY_TIMEOUT_S`].
pub fn deferred_verify_busy_queued(
    deps: &dyn DeferredVerifyDeps,
    message: &str,
    timeout_s: u64,
    poll_ms: u64,
) -> PayloadVerifyOutcome {
    deferred_verify_floor_core(
        deps,
        message,
        timeout_s,
        poll_ms,
        "busy-queued transcript never resolved within the deferred window",
    )
}

/// The LONGEST user-record text carrying the same-turn truncation signature
/// against `message` (the SAME `carries_truncation_signature` logic
/// [`verify_chunked_payload`] uses to decide `Truncated`), or `None` if none
/// qualifies. ACK-2 (§2.3.4): after a `Truncated` outcome the verb re-reads the
/// user texts past the offset and shas THIS record for the
/// `turn-anchored-mismatch.actual_sha` field — `PayloadVerifyOutcome::Truncated`
/// carries lengths only, so the actual_sha is computed honestly at the call
/// site. PURE (no fs/clock); does not touch the verify trait or core.
pub fn longest_truncation_signature<'a>(texts: &'a [String], message: &str) -> Option<&'a str> {
    texts
        .iter()
        .filter(|r| carries_truncation_signature(r, message))
        .max_by_key(|r| r.len())
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Virtual-time deps (port of `fakeDeps`,
    /// qa/hardening@3dd9f1e:src/submit.test.ts:25-43). `status_at(elapsed_ms,
    /// crs_fired)` models the session's status timeline; `sleep` advances the
    /// virtual clock; `send_cr` is counted (and timed).
    struct FakeDeps<F: Fn(i64, u32) -> Option<&'static str>> {
        t: Cell<i64>,
        crs: Cell<u32>,
        cr_times: RefCell<Vec<i64>>,
        status_at: F,
    }
    impl<F: Fn(i64, u32) -> Option<&'static str>> FakeDeps<F> {
        fn new(status_at: F) -> Self {
            Self {
                t: Cell::new(0),
                crs: Cell::new(0),
                cr_times: RefCell::new(Vec::new()),
                status_at,
            }
        }
        fn crs(&self) -> u32 {
            self.crs.get()
        }
    }
    impl<F: Fn(i64, u32) -> Option<&'static str>> SubmitDeps for FakeDeps<F> {
        fn read_status(&self) -> Option<String> {
            (self.status_at)(self.t.get(), self.crs.get()).map(str::to_string)
        }
        fn send_cr(&self) {
            self.crs.set(self.crs.get() + 1);
            self.cr_times.borrow_mut().push(self.t.get());
        }
        fn sleep(&self, ms: u64) {
            self.t.set(self.t.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
    }

    // --- ported submit.test.ts §5 cases ----------------------------------

    #[test]
    fn d_short_auto_submits_within_settle_zero_crs() {
        // Short write: goes busy ~1.5s, inside the ~2s settle window.
        let f = FakeDeps::new(|t, _| {
            if t >= 1500 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = verify_accepted_then_cr(&f, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 0); // it submitted on its own — no remediation
        assert_eq!(f.crs(), 0);
    }

    #[test]
    fn d_600_large_write_exactly_one_cr_submits_it() {
        // The ~600-char write is stuck (paste-burst absorbed the \r): never busy
        // until a CR fires; after the CR it goes busy.
        let f = FakeDeps::new(|_t, crs| if crs >= 1 { Some("busy") } else { Some("idle") });
        let out = verify_accepted_then_cr(&f, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 1); // exactly one remediation CR
        assert_eq!(f.crs(), 1);
    }

    #[test]
    fn d_longresp_busy_at_settle_response_runs_long_no_stray_cr() {
        // Keys on ACCEPTANCE: goes busy at 1.5s (within settle) and STAYS busy far
        // past the window (long response). Must classify accepted and NOT fire a
        // CR during the long-running response (the NEW-2 regression).
        let f = FakeDeps::new(|t, _| {
            if t >= 1500 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = verify_accepted_then_cr(
            &f,
            SubmitOptions {
                settle_ms: 2000,
                post_cr_ms: 30_000,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 0);
        assert_eq!(f.crs(), 0); // no stray CR into the still-running response
    }

    #[test]
    fn d_never_cr_busy_already_busy_session_never_gets_a_cr() {
        let f = FakeDeps::new(|_, _| Some("busy"));
        let out = verify_accepted_then_cr(&f, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(f.crs(), 0); // raw zmx send wouldn't refuse it — the guard must
    }

    #[test]
    fn d_no_blanket_already_submitted_short_prompt_no_second_cr() {
        // Auto-submitted (busy in settle). A blanket post-send CR would
        // double-submit → a spurious empty turn. Assert exactly zero CRs.
        let f = FakeDeps::new(|t, _| if t >= 800 { Some("busy") } else { Some("idle") });
        let out = verify_accepted_then_cr(&f, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 0);
        assert_eq!(f.crs(), 0);
    }

    #[test]
    fn stuck_even_after_cr_reports_not_accepted() {
        // Never goes busy, even after the remediation CR. Exactly one CR is fired
        // (never more), and the outcome is not-accepted so callers exit 1.
        let f = FakeDeps::new(|_, _| Some("idle"));
        let out = verify_accepted_then_cr(
            &f,
            SubmitOptions {
                settle_ms: SubmitOptions::default().settle_ms,
                post_cr_ms: 4000,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        assert!(!out.accepted);
        assert_eq!(out.crs_fired, 1); // exactly one CR, never a barrage
        assert_eq!(f.crs(), 1);
    }

    // --- Rust additions (a4-spec §2.1 / D) -------------------------------

    #[test]
    fn busy_at_boundary_suppresses_cr() {
        // The session is idle through the WHOLE settle window (every settle-loop
        // read sees idle and the window elapses to false), then crosses into busy
        // at EXACTLY the re-read-once-more-before-acting step. The CR MUST be
        // suppressed (suppressed_busy_cr) and zero CRs fired.
        //
        // Time-keyed status can't model this: the re-read happens at the same
        // virtual instant the settle loop's final (false) read did, so a t-keyed
        // flip can't fire only on the re-read. We key on READ COUNT instead: the
        // settle window does a bounded number of reads (settle 2500 / poll 250 →
        // 11 reads at t=0..2500), all idle; the very NEXT read — the re-read —
        // returns busy. This is precisely the boundary-crossing the suppress path
        // defends.
        let reads = std::cell::Cell::new(0u32);
        let f = FakeDeps::new(move |_t, _| {
            let n = reads.get();
            reads.set(n + 1);
            // First 11 reads (the full settle window) idle; the 12th (re-read) busy.
            if n >= 11 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = verify_accepted_then_cr(&f, SubmitOptions::default());
        assert!(out.accepted);
        assert!(out.suppressed_busy_cr);
        assert_eq!(out.crs_fired, 0);
        assert_eq!(f.crs(), 0);
    }

    #[test]
    fn status_source_vanishes_none_is_not_busy_no_panic() {
        // read_status returns None (PID file gone) for the whole window. None is
        // NEVER busy — the discipline must return not-accepted without panicking,
        // firing exactly one CR (the remediation), then still not-accepted.
        let f = FakeDeps::new(|_, _| None);
        let out = verify_accepted_then_cr(
            &f,
            SubmitOptions {
                settle_ms: SubmitOptions::default().settle_ms,
                post_cr_ms: 2000,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        assert!(!out.accepted);
        assert_eq!(out.crs_fired, 1);
    }

    #[test]
    fn status_source_vanishes_mid_wait_then_recovers_busy() {
        // None mid-wait must not be read as busy; once the source returns busy the
        // settle window still accepts. Idle→None→busy across the window.
        let f = FakeDeps::new(|t, _| {
            if t < 500 {
                Some("idle")
            } else if t < 1500 {
                None
            } else {
                Some("busy")
            }
        });
        let out = verify_accepted_then_cr(&f, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 0);
    }

    #[test]
    fn zero_settle_window_still_does_one_read() {
        // settle_ms 0: the immediate first read must still happen (wait_for_busy
        // reads before checking the deadline). Busy at t=0 → accepted, zero CRs.
        let f = FakeDeps::new(|_, _| Some("busy"));
        let out = verify_accepted_then_cr(
            &f,
            SubmitOptions {
                settle_ms: 0,
                post_cr_ms: 1000,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 0);
    }

    #[test]
    fn zero_settle_window_idle_does_one_read_then_remediates() {
        // settle_ms 0, idle at t=0: one read (idle), deadline check false, re-read
        // (still idle) → CR fires. Proves the zero-window still reads exactly once
        // before remediating (it doesn't skip straight to CR or loop forever).
        let f = FakeDeps::new(|_t, crs| if crs >= 1 { Some("busy") } else { Some("idle") });
        let out = verify_accepted_then_cr(
            &f,
            SubmitOptions {
                settle_ms: 0,
                post_cr_ms: 1000,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 1);
    }

    // --- R4 two-write idle delivery (ADR 0009) ----------------------------

    /// A fake [`IdleDeliverDeps`] recording the write ORDER (text vs CR) + timings,
    /// driving status from `status_at(elapsed_ms, crs_fired)` and returning a
    /// scripted composer screen. Models the live R4 mechanism without a real PTY.
    struct FakeIdle<F: Fn(i64, u32) -> Option<&'static str>> {
        t: Cell<i64>,
        crs: Cell<u32>,
        /// Recorded write order: ("text", payload) and ("cr", "") events.
        writes: RefCell<Vec<(&'static str, String)>>,
        /// What the composer screen holds (the content-verified CR keys on this).
        screen: RefCell<String>,
        /// When the screen "clears" (composer empties) after the Nth CR fires; the
        /// content-verified path must then stop CRing. None → never clears.
        clear_screen_after_crs: Option<u32>,
        status_at: F,
    }
    impl<F: Fn(i64, u32) -> Option<&'static str>> FakeIdle<F> {
        fn new(_message: &str, screen: &str, status_at: F) -> Self {
            Self {
                t: Cell::new(0),
                crs: Cell::new(0),
                writes: RefCell::new(Vec::new()),
                screen: RefCell::new(screen.to_string()),
                clear_screen_after_crs: None,
                status_at,
            }
        }
        fn crs(&self) -> u32 {
            self.crs.get()
        }
        fn write_kinds(&self) -> Vec<&'static str> {
            self.writes.borrow().iter().map(|(k, _)| *k).collect()
        }
    }
    impl<F: Fn(i64, u32) -> Option<&'static str>> IdleDeliverDeps for FakeIdle<F> {
        fn send_text(&self, text: &str) {
            self.writes.borrow_mut().push(("text", text.to_string()));
        }
        fn send_cr(&self) {
            self.writes.borrow_mut().push(("cr", String::new()));
            let n = self.crs.get() + 1;
            self.crs.set(n);
            if self.clear_screen_after_crs == Some(n) {
                // The CR submitted the composer → it no longer holds the message.
                *self.screen.borrow_mut() = "\u{276f} ".to_string();
            }
        }
        fn read_screen(&self) -> String {
            self.screen.borrow().clone()
        }
        fn read_status(&self) -> Option<String> {
            (self.status_at)(self.t.get(), self.crs.get()).map(str::to_string)
        }
        fn sleep(&self, ms: u64) {
            self.t.set(self.t.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
    }

    fn screen_holding(msg: &str) -> String {
        format!("\u{276f} {msg}")
    }

    #[test]
    fn idle_two_write_order_is_text_then_cr_then_no_remediation_when_accepted() {
        // The two writes are TEXT first, then a SEPARATE CR. The session goes busy
        // off the two-write CR (within the settle window) → NO remediation CR.
        let msg = "hello world";
        let f = FakeIdle::new(msg, &screen_holding(msg), |_t, crs| {
            // Busy once the two-write CR (crs==0 still, the two-write CR is NOT
            // counted via send_cr? it IS — send_cr increments). Model: busy after
            // the first CR write.
            if crs >= 1 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = deliver_idle_two_write(&f, msg, SubmitOptions::default());
        assert!(out.accepted);
        // Write order: text, then cr (the two-write). No remediation CR (busy in
        // settle).
        assert_eq!(f.write_kinds(), vec!["text", "cr"]);
        assert_eq!(out.crs_fired, 0, "auto-accepted off the two-write CR");
        // crs counter = 1 (the two-write CR only); the remediation never fired.
        assert_eq!(f.crs(), 1);
    }

    #[test]
    fn idle_two_write_content_verified_remediation_fires_when_stuck() {
        // The two-write CR is ALSO absorbed (the live R4 worst case): the composer
        // still holds the message. The acceptance check sees not-busy, the
        // content-verified CR sees the message present → fires ONE remediation CR,
        // after which the session goes busy.
        let msg = "P".repeat(4096);
        let f = FakeIdle::new(&msg, &screen_holding(&msg), |_t, crs| {
            // Not busy off the two-write CR (crs==1); busy only after the
            // remediation CR (crs==2).
            if crs >= 2 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = deliver_idle_two_write(&f, &msg, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 1, "exactly one remediation CR");
        // Writes: the 4096B message chunks into 4×1024B text writes (mode (a)
        // chunking), then cr (two-write), then cr (remediation). The chunking only
        // subdivides the TEXT phase; the CR discipline is unchanged.
        assert_eq!(
            f.write_kinds(),
            vec!["text", "text", "text", "text", "cr", "cr"]
        );
        assert_eq!(f.crs(), 2);
    }

    #[test]
    fn idle_two_write_never_blind_cr_when_composer_empty() {
        // The message already submitted (two-write CR worked) but the status poll
        // is slow to flip — model: composer is EMPTY (does not hold the message),
        // yet status still idle through the settle window. The content-verified CR
        // must NOT fire (never blind), so NO remediation CR despite not-busy.
        let msg = "already gone";
        // Screen does NOT hold the message (empty composer after the ❯).
        let f = FakeIdle::new(msg, "\u{276f} ", |_t, _crs| Some("idle"));
        let out = deliver_idle_two_write(
            &f,
            msg,
            SubmitOptions {
                settle_ms: SubmitOptions::default().settle_ms,
                post_cr_ms: 1000,
                poll_ms: SubmitOptions::default().poll_ms,
            },
        );
        // Not accepted (status never flips), but the CR was SUPPRESSED by the
        // content check — only the two-write CR was emitted, zero remediation CRs.
        assert!(!out.accepted);
        assert_eq!(
            f.write_kinds(),
            vec!["text", "cr"],
            "no blind remediation CR when the composer does not hold the message"
        );
        assert_eq!(f.crs(), 1, "only the two-write CR; remediation suppressed");
    }

    #[test]
    fn idle_two_write_never_cr_busy() {
        // The session is already busy through the whole flow. verify_accepted_then_cr
        // never CRs a busy session; combined with content-verification, the only CR
        // is the two-write CR itself (the delivery keystroke).
        let msg = "x";
        let f = FakeIdle::new(msg, &screen_holding(msg), |_t, _crs| Some("busy"));
        let out = deliver_idle_two_write(&f, msg, SubmitOptions::default());
        assert!(out.accepted);
        assert_eq!(out.crs_fired, 0);
        assert_eq!(f.crs(), 1, "two-write CR only; no remediation into busy");
    }

    #[test]
    fn idle_two_write_remediation_stops_when_screen_clears() {
        // The remediation CR submits the composer; the screen then clears. A
        // hypothetical further CR (there is at most one in this discipline) would
        // see an empty composer and not fire — proving the content gate tracks the
        // composer's live state.
        let msg = "submit me";
        let mut f = FakeIdle::new(msg, &screen_holding(msg), |_t, crs| {
            // Stays idle even after the remediation CR (status lags) so we can prove
            // the content gate — not the busy gate — is what stops a second CR.
            let _ = crs;
            Some("idle")
        });
        f.clear_screen_after_crs = Some(2); // the remediation CR (2nd CR) clears it
        let out = deliver_idle_two_write(
            &f,
            msg,
            SubmitOptions {
                settle_ms: 400,
                post_cr_ms: 400,
                poll_ms: 100,
            },
        );
        // verify_accepted_then_cr fires AT MOST one remediation CR regardless; this
        // asserts the content gate would suppress further ones (defense-in-depth).
        assert!(!out.accepted);
        assert_eq!(
            out.crs_fired, 1,
            "the discipline caps remediation at one CR"
        );
        assert_eq!(f.crs(), 2, "two-write CR + one remediation CR");
    }

    // --- deliver_prompt 3-way outcomes (a4-spec §2.2) --------------------

    /// A fake [`DeliverDeps`] driving a captured timeline. `pid` controls the
    /// find_pid_file outcome; `status_at` drives the inner discipline. The CR
    /// counter is SHARED with the per-round SubmitDeps so the bounded-retry loop's
    /// total CR count is observable.
    ///
    /// R4 (ADR 0009): `submit_deps` returns a real [`ContentVerifiedSubmit`] over a
    /// fake [`IdleDeliverDeps`], so these tests exercise the PRODUCTION
    /// content-verified wrapping. `screen_holds` controls whether the composer holds
    /// the message (default true → the CR is content-verified-PASS and fires; a
    /// false screen would suppress every CR — exercised by a dedicated test).
    struct FakeDeliver {
        pid: Option<PathBuf>,
        send_count: Cell<u32>,
        crs: Cell<u32>,
        screen_holds: bool,
        status_at: Box<dyn Fn(i64, u32) -> Option<&'static str>>,
    }
    impl FakeDeliver {
        fn new(
            pid: Option<PathBuf>,
            status_at: impl Fn(i64, u32) -> Option<&'static str> + 'static,
        ) -> Self {
            Self {
                pid,
                send_count: Cell::new(0),
                crs: Cell::new(0),
                screen_holds: true,
                status_at: Box::new(status_at),
            }
        }
    }
    /// An [`IdleDeliverDeps`] sharing the parent's CR counter + virtual clock. Each
    /// round gets its own clock (each `verify_accepted_then_cr` recomputes its
    /// deadline from `now_ms()`; status_at keys on total_crs, not absolute t, so a
    /// per-round clock reset is correct here). `read_screen` returns the message
    /// when `screen_holds` so the content-verified CR fires.
    struct SharedIdle<'a> {
        t: Cell<i64>,
        message: String,
        parent: &'a FakeDeliver,
    }
    impl IdleDeliverDeps for SharedIdle<'_> {
        fn send_text(&self, _text: &str) {}
        fn send_cr(&self) {
            self.parent.crs.set(self.parent.crs.get() + 1);
        }
        fn read_screen(&self) -> String {
            // Compose a screen that holds the message after a ❯ glyph (so
            // composer_holds_message is true), or an empty composer otherwise.
            if self.parent.screen_holds {
                format!("\u{276f} {}", self.message)
            } else {
                "\u{276f} ".to_string()
            }
        }
        fn read_status(&self) -> Option<String> {
            (self.parent.status_at)(self.t.get(), self.parent.crs.get()).map(str::to_string)
        }
        fn sleep(&self, ms: u64) {
            self.t.set(self.t.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
    }
    impl DeliverDeps for FakeDeliver {
        fn send_message(&self, _message: &str) {
            self.send_count.set(self.send_count.get() + 1);
        }
        fn read_screen(&self) -> String {
            // Not consulted directly (the per-round content-verification lives in
            // SharedIdle::read_screen); present for trait completeness.
            String::new()
        }
        fn find_pid_file(&self) -> Option<PathBuf> {
            self.pid.clone()
        }
        fn submit_deps(&self, _pid_file: PathBuf, message: &str) -> Box<dyn SubmitDeps + '_> {
            Box::new(ContentVerifiedSubmit::new(
                SharedIdle {
                    t: Cell::new(0),
                    message: message.to_string(),
                    parent: self,
                },
                message,
            ))
        }
    }

    #[test]
    fn deliver_prompt_accepted_first_round() {
        // Goes busy in the first settle window → Accepted, one send, zero CRs.
        let d = FakeDeliver::new(Some(PathBuf::from("/x/1.json")), |t, _| {
            if t >= 1000 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        assert_eq!(
            deliver_prompt(&d, "hi", DELIVER_TIMEOUT_S),
            DeliverOutcome::Accepted
        );
        assert_eq!(d.send_count.get(), 1);
        assert_eq!(d.crs.get(), 0);
    }

    #[test]
    fn deliver_prompt_pid_file_missing_is_distinct() {
        // find_pid_file None → PidFileMissing, NOT Stalled (R1). No CRs (we never
        // reached the discipline).
        let d = FakeDeliver::new(None, |_, _| Some("idle"));
        assert_eq!(
            deliver_prompt(&d, "hi", DELIVER_TIMEOUT_S),
            DeliverOutcome::PidFileMissing
        );
        assert_eq!(d.crs.get(), 0);
    }

    #[test]
    fn deliver_prompt_stalled_after_full_remediation() {
        // PID readable but NEVER goes busy, even after every CR → Stalled.
        let d = FakeDeliver::new(Some(PathBuf::from("/x/1.json")), |_, _| Some("idle"));
        assert_eq!(
            deliver_prompt(&d, "hi", DELIVER_TIMEOUT_S),
            DeliverOutcome::Stalled
        );
    }

    #[test]
    fn deliver_prompt_retry_rounds_bounded_at_1_plus_3_crs_max() {
        // Never goes busy → the first round fires 1 CR, then max_rounds=3 extra
        // rounds fire 1 CR each: 4 CRs total, never more. Bounded (never
        // blanket-CR).
        let d = FakeDeliver::new(Some(PathBuf::from("/x/1.json")), |_, _| Some("idle"));
        let out = deliver_prompt(&d, "hi", DELIVER_TIMEOUT_S);
        assert_eq!(out, DeliverOutcome::Stalled);
        assert_eq!(
            d.crs.get(),
            4,
            "1 first-round CR + 3 retry-round CRs, capped"
        );
    }

    #[test]
    fn deliver_prompt_accepted_on_a_retry_round() {
        // Stuck through the first round + first two retries; the 3rd CR submits it
        // (busy once 3 CRs have fired). Accepted, exactly 3 CRs.
        let d = FakeDeliver::new(Some(PathBuf::from("/x/1.json")), |_, crs| {
            if crs >= 3 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = deliver_prompt(&d, "hi", DELIVER_TIMEOUT_S);
        assert_eq!(out, DeliverOutcome::Accepted);
        assert_eq!(d.crs.get(), 3);
    }

    // --- chunk_text — character-boundary-safe ≤N-byte chunking ------------
    // Vectors ported from 8c59ec4:src/submit.test.ts:387-426 + Rust additions.
    // Invariants asserted on every chunk vector: (1) concat of chunks == original
    // BYTES; (2) every chunk ≤ chunk_bytes (except a lone over-budget code point);
    // (3) every chunk is valid UTF-8 (trivially: chunks are &str); (4) the code-point
    // count is preserved (no char dropped, doubled, or split).

    /// Assert the universal chunk_text invariants for `msg` at budget `n`.
    fn assert_chunk_invariants(msg: &str, n: usize, chunks: &[&str]) {
        // (1) exact byte reassembly, in order.
        assert_eq!(chunks.concat(), msg, "chunks must reassemble byte-for-byte");
        // (4) code-point count preserved.
        assert_eq!(
            chunks.iter().map(|c| c.chars().count()).sum::<usize>(),
            msg.chars().count(),
            "no code point dropped/doubled/split"
        );
        for c in chunks {
            // (3) valid UTF-8 is structural (it's a &str), but assert no chunk ends
            // mid-character by round-tripping bytes → str.
            assert_eq!(
                std::str::from_utf8(c.as_bytes()).unwrap(),
                *c,
                "chunk must be a whole-character &str"
            );
            // (2) ≤ budget, UNLESS the chunk is a single over-budget code point.
            let only_one_cp = c.chars().count() == 1;
            assert!(
                c.len() <= n || only_one_cp,
                "chunk len {} exceeds budget {n} and is not a lone code point",
                c.len()
            );
        }
    }

    #[test]
    fn chunk_text_empty_message_no_chunks() {
        // 8c59ec4:src/submit.test.ts:388 — caller writes nothing for the empty string.
        let chunks = chunk_text("", 1024);
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_text_smaller_than_one_chunk_is_single_chunk() {
        // 8c59ec4:src/submit.test.ts:392 — small-send, no behavior change (one chunk).
        assert_eq!(chunk_text("hello", 1024), vec!["hello"]);
    }

    #[test]
    fn chunk_text_exact_1024_ascii_boundary() {
        // 8c59ec4:src/submit.test.ts:396 — ASCII at the exact byte boundary. 4200 P's
        // at 1024 → 5 chunks (1024*4 + 104), each ≤1024, reassembling exactly.
        let msg = "A".repeat(4200);
        let chunks = chunk_text(&msg, 1024);
        assert_eq!(chunks.len(), 5, "1024*4 + 104 = 5 chunks");
        assert_chunk_invariants(&msg, 1024, &chunks);
    }

    #[test]
    fn chunk_text_exact_full_chunks_no_remainder() {
        // Rust addition: an EXACT multiple of the budget yields exactly that many
        // full chunks with no short trailing chunk (the boundary edge: cur_bytes ==
        // max then the next code point opens a new chunk).
        let msg = "A".repeat(1024 * 3);
        let chunks = chunk_text(&msg, 1024);
        assert_eq!(
            chunks.len(),
            3,
            "3*1024 → exactly 3 full chunks, no remainder"
        );
        for c in &chunks {
            assert_eq!(c.len(), 1024, "every chunk is a full 1024 bytes");
        }
        assert_chunk_invariants(&msg, 1024, &chunks);
    }

    #[test]
    fn chunk_text_multibyte_straddles_boundary_char_intact() {
        // 8c59ec4:src/submit.test.ts:403 — a chunk edge landing mid-multibyte must
        // never split a char. "é" is 2 UTF-8 bytes; with a budget of 5 the 2nd "é"
        // would straddle a naive byte slice. Assert every chunk is whole-char and the
        // boundary chunk runs SHORT rather than splitting "é" (B3 F-2: boundary bugs
        // are live — be exact).
        let msg = "é".repeat(10); // 20 bytes, 10 code points
        let chunks = chunk_text(&msg, 5); // 5 is odd → forces a mid-char would-be split
        assert_chunk_invariants(&msg, 5, &chunks);
        // At budget 5, two "é" (4 bytes) fit, a third (6 bytes) overflows → each chunk
        // holds exactly 2 é's (4 bytes), running 1 byte SHORT of the 5-byte budget.
        for c in &chunks {
            assert_eq!(
                c.len(),
                4,
                "chunk runs short (4<5) — never splits the 2-byte é"
            );
        }
    }

    #[test]
    fn chunk_text_emoji_4byte_straddles_boundary() {
        // 8c59ec4:src/submit.test.ts:403 analogue — a 4-byte emoji (☕ is 3 bytes, 😀
        // is 4) straddling a budget that is NOT a multiple of the char width. Mixed
        // widths so several boundaries land mid-character; assert all intact.
        let msg = "😀".repeat(10) + &"é".repeat(10) + &"x".repeat(10);
        let chunks = chunk_text(&msg, 7); // 7 ∤ 4 and 7 ∤ 2 → boundaries fall mid-char
        assert_chunk_invariants(&msg, 7, &chunks);
        assert!(chunks.len() > 1, "genuinely chunked");
    }

    #[test]
    fn chunk_text_single_codepoint_over_budget_goes_out_whole() {
        // 8c59ec4:src/submit.test.ts:419 — "😀" is 4 bytes; with a 2-byte budget it
        // cannot fit but must NOT be split. It goes out ALONE, over budget.
        assert_eq!(chunk_text("😀", 2), vec!["😀"]);
    }

    #[test]
    fn chunk_text_cjk_payload_boundaries_intact() {
        // Rust addition (the fakerepl straddle payload): 構築日本語café☕ repeated. CJK
        // glyphs are 3 bytes each, café mixes 1+2-byte, ☕ is 3 bytes. A small budget
        // forces boundaries through every width class; assert byte-exact + char-exact.
        let unit = "構築日本語café☕";
        let msg = unit.repeat(40);
        for &n in &[1usize, 2, 3, 4, 5, 7, 8, 16, 1024] {
            let chunks = chunk_text(&msg, n);
            assert_chunk_invariants(&msg, n, &chunks);
        }
    }

    #[test]
    fn chunk_text_budget_zero_clamps_to_one() {
        // Rust addition: a 0 budget clamps to 1 (max(1)) — every code point goes out
        // as its own chunk rather than dividing by zero / never advancing.
        let msg = "abé😀";
        let chunks = chunk_text(msg, 0);
        assert_eq!(
            chunks.len(),
            msg.chars().count(),
            "one chunk per code point"
        );
        assert_chunk_invariants(msg, 1, &chunks);
    }

    // --- send_text_chunked — ordered ≤N-byte writes with inter-chunk delay -

    /// Record each chunk send() and count sleeps (a virtual clock).
    fn recorder() -> (Vec<String>, u32) {
        (Vec::new(), 0)
    }

    #[test]
    fn send_text_chunked_large_arrives_as_multiple_ordered_writes() {
        // 8c59ec4:src/submit.test.ts:440 — >chunk_bytes arrives as multiple send()
        // calls, each ≤ the chunk limit, in order, reassembling, with exactly one
        // delay BETWEEN chunks (not before the first, not after the last).
        let msg = "X".repeat(4300);
        let (mut sent, mut sleeps) = recorder();
        send_text_chunked(
            &mut |c| sent.push(c.to_string()),
            &mut |_ms| sleeps += 1,
            &msg,
            ChunkSendOptions {
                chunk_bytes: 1024,
                settle_ms: 150,
            },
        );
        assert!(sent.len() > 1, "genuinely chunked");
        for c in &sent {
            assert!(c.len() <= 1024, "each chunk ≤ the limit");
        }
        assert_eq!(sent.concat(), msg, "ordered, exact reassembly");
        assert_eq!(
            sleeps,
            (sent.len() - 1) as u32,
            "one delay BETWEEN chunks (n-1 sleeps)"
        );
    }

    #[test]
    fn send_text_chunked_one_chunk_single_write_no_delay() {
        // 8c59ec4:src/submit.test.ts:454 — a message that fits one chunk is a single
        // send() with NO inter-chunk delay (byte-identical to the prior single write).
        let (mut sent, mut sleeps) = recorder();
        send_text_chunked(
            &mut |c| sent.push(c.to_string()),
            &mut |_ms| sleeps += 1,
            "small",
            ChunkSendOptions::default(),
        );
        assert_eq!(sent, vec!["small".to_string()]);
        assert_eq!(sleeps, 0, "no inter-chunk sleep for a single chunk");
    }

    #[test]
    fn send_text_chunked_empty_message_no_send() {
        // 8c59ec4:src/submit.test.ts:461 — an empty message is a no-op: no send at all.
        let (mut sent, mut sleeps) = recorder();
        send_text_chunked(
            &mut |c| sent.push(c.to_string()),
            &mut |_ms| sleeps += 1,
            "",
            ChunkSendOptions::default(),
        );
        assert!(sent.is_empty(), "no send for the empty message");
        assert_eq!(sleeps, 0);
    }

    #[test]
    fn send_text_chunked_settle_ms_value_is_used() {
        // Rust addition: the injected settle_ms is the value slept between chunks.
        let msg = "Y".repeat(2049); // 3 chunks at 1024 → 2 sleeps
        let mut slept: Vec<u64> = Vec::new();
        let mut sent: Vec<String> = Vec::new();
        send_text_chunked(
            &mut |c| sent.push(c.to_string()),
            &mut |ms| slept.push(ms),
            &msg,
            ChunkSendOptions {
                chunk_bytes: 1024,
                settle_ms: 42,
            },
        );
        assert_eq!(sent.len(), 3);
        assert_eq!(
            slept,
            vec![42, 42],
            "exactly the injected settle, between chunks"
        );
    }

    #[test]
    fn idle_two_write_with_chunks_then_cr_order() {
        // The chunked text phase drives the trait's send_text once PER CHUNK, then a
        // SINGLE separate CR — so a ≥4KB idle delivery is N text writes + 1 cr (not a
        // single text write). Proves chunking lands in the shared idle helper.
        let msg = "Z".repeat(4096); // 4 chunks at 1024
        let f = FakeIdle::new(&msg, &screen_holding(&msg), |_t, crs| {
            // Busy off the two-write CR (the FIRST cr, which is the 5th send_cr? no —
            // send_cr only counts CR writes; chunks go via send_text). Busy after CR 1.
            if crs >= 1 {
                Some("busy")
            } else {
                Some("idle")
            }
        });
        let out = deliver_idle_two_write(&f, &msg, SubmitOptions::default());
        assert!(out.accepted);
        let kinds = f.write_kinds();
        // 4 text chunk writes, then 1 cr (the two-write submit). No remediation.
        assert_eq!(
            kinds,
            vec!["text", "text", "text", "text", "cr"],
            "chunked text (4 writes) then a separate CR"
        );
        assert_eq!(out.crs_fired, 0, "accepted off the two-write CR");
    }

    // --- W8 verify-after-submit (wart-wave §4) ---------------------------

    /// A scripted [`VerifyDeps`]: each poll pops the next entry from a queue of
    /// read RESULTS (`Ok(texts)` or `Err(reason)`); once the queue drains, the
    /// LAST entry repeats (models a steady-state transcript). `sleep` advances a
    /// virtual clock so the budget is deterministic without wall time.
    struct FakeVerify {
        t: Cell<i64>,
        results: RefCell<std::collections::VecDeque<Result<Vec<String>, String>>>,
        reads: Cell<u32>,
    }
    impl FakeVerify {
        fn new(results: Vec<Result<Vec<String>, String>>) -> Self {
            Self {
                t: Cell::new(0),
                results: RefCell::new(results.into_iter().collect()),
                reads: Cell::new(0),
            }
        }
        fn reads(&self) -> u32 {
            self.reads.get()
        }
    }
    impl VerifyDeps for FakeVerify {
        fn read_user_texts(&self) -> Result<Vec<String>, String> {
            self.reads.set(self.reads.get() + 1);
            let mut q = self.results.borrow_mut();
            if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                // Last entry repeats (steady state); empty queue → no record.
                q.front().cloned().unwrap_or_else(|| Ok(Vec::new()))
            }
        }
        fn sleep(&self, ms: u64) {
            self.t.set(self.t.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
    }

    fn ok(texts: &[&str]) -> Result<Vec<String>, String> {
        Ok(texts.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn verify_exact_match_first_poll_is_verified() {
        let msg = "hello world";
        let f = FakeVerify::new(vec![ok(&[msg])]);
        let out = verify_chunked_payload(&f, msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(out, PayloadVerifyOutcome::Verified);
        assert_eq!(f.reads(), 1, "matched on the first read — no extra polls");
    }

    #[test]
    fn verify_exact_match_on_a_later_poll_is_verified() {
        // Empty on the first reads (record not landed yet), then it appears.
        let msg = "delayed payload";
        let f = FakeVerify::new(vec![ok(&[]), ok(&[]), ok(&[msg])]);
        let out = verify_chunked_payload(&f, msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(out, PayloadVerifyOutcome::Verified);
        assert_eq!(f.reads(), 3, "polled until the record resolved");
    }

    #[test]
    fn verify_truncated_prefix_candidate_reports_truncated() {
        // A prefix-truncation: the record is the leading half of the message.
        let msg = "A".repeat(2048);
        let recorded = "A".repeat(1024); // shorter, shares all leading bytes
        let f = FakeVerify::new(vec![ok(&[&recorded])]);
        let out = verify_chunked_payload(&f, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(
            out,
            PayloadVerifyOutcome::Truncated {
                expected: 2048,
                recorded: 1024,
            }
        );
    }

    #[test]
    fn verify_mid_loss_shape_still_truncated() {
        // MID-LOSS: a 16KB message; the record is first-1KB ++ last-1KB (2KB).
        // It is SHORTER and still shares the leading 64 bytes → truncation
        // signature fires (covers mid-payload drop, not just a clean prefix cut).
        let msg = format!("{}{}", "H".repeat(8 * 1024), "T".repeat(8 * 1024));
        let recorded = format!("{}{}", "H".repeat(1024), "T".repeat(1024));
        let f = FakeVerify::new(vec![ok(&[&recorded])]);
        let out = verify_chunked_payload(&f, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(
            out,
            PayloadVerifyOutcome::Truncated {
                expected: 16 * 1024,
                recorded: 2 * 1024,
            }
        );
    }

    #[test]
    fn verify_longest_truncation_candidate_wins() {
        // Two truncation candidates past the offset → the LONGEST (best evidence).
        let msg = "P".repeat(4096);
        let short = "P".repeat(1000);
        let long = "P".repeat(3000);
        let f = FakeVerify::new(vec![ok(&[&short, &long])]);
        let out = verify_chunked_payload(&f, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(
            out,
            PayloadVerifyOutcome::Truncated {
                expected: 4096,
                recorded: 3000,
            }
        );
    }

    #[test]
    fn verify_foreign_record_only_is_unattributable() {
        // A record exists past the offset but it is unrelated (no shared prefix,
        // and not shorter-with-prefix) → degrade, NOT truncation.
        let msg = "the original payload text";
        let foreign = "a totally different user message";
        let f = FakeVerify::new(vec![ok(&[foreign])]);
        let out = verify_chunked_payload(&f, msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(out, PayloadVerifyOutcome::Unattributable);
    }

    #[test]
    fn verify_foreign_plus_exact_is_verified() {
        // Exact match WINS even alongside a foreign record (R8: match beats noise).
        let msg = "the original payload text";
        let foreign = "a totally different user message";
        let f = FakeVerify::new(vec![ok(&[foreign, msg])]);
        let out = verify_chunked_payload(&f, msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(out, PayloadVerifyOutcome::Verified);
    }

    #[test]
    fn verify_empty_reads_is_no_record() {
        // Reads succeed but NO user record ever appears in the budget.
        let msg = "P".repeat(2048);
        let f = FakeVerify::new(vec![ok(&[])]);
        let out = verify_chunked_payload(&f, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(out, PayloadVerifyOutcome::NoRecord);
    }

    #[test]
    fn verify_always_err_is_source_unavailable() {
        // EVERY read errors (resolution never succeeds) → SourceUnavailable with
        // the last reason; NEVER a false truncation/no-record.
        let msg = "P".repeat(2048);
        let f = FakeVerify::new(vec![Err("session_id unresolvable".to_string())]);
        let out = verify_chunked_payload(&f, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(
            out,
            PayloadVerifyOutcome::SourceUnavailable("session_id unresolvable".to_string())
        );
    }

    #[test]
    fn verify_err_then_success_uses_the_successful_read() {
        // A transient read failure must NOT become SourceUnavailable if a later
        // read succeeds: an Err early, then an empty Ok → NoRecord (a read DID
        // succeed), not SourceUnavailable.
        let msg = "P".repeat(2048);
        let f = FakeVerify::new(vec![Err("file shrank".to_string()), ok(&[])]);
        let out = verify_chunked_payload(&f, &msg, VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(out, PayloadVerifyOutcome::NoRecord);
    }

    // --- trigger-predicate seam rows (red-team R3) -----------------------

    #[test]
    fn needs_verify_1024_ascii_false_1025_true() {
        // The seam is the CHUNK COUNT from the production splitter, not a byte
        // count: exactly 1024 ASCII bytes is ONE chunk (no verify); 1025 is TWO.
        assert!(
            !payload_needs_verify(&"a".repeat(1024)),
            "1024 ASCII bytes = one chunk → no verify (scope guard)"
        );
        assert!(
            payload_needs_verify(&"a".repeat(1025)),
            "1025 ASCII bytes = two chunks → verify"
        );
    }

    #[test]
    fn needs_verify_multibyte_chunk_count_decides_not_byte_count() {
        // 1023 ASCII + one 3-byte code point = 1026 BYTES. A byte-count gloss
        // (">1024B") would say "verify". But the chunker never splits a code
        // point: the 3-byte char cannot join the 1023-byte run (1023+3 > 1024), so
        // it starts a SECOND chunk → chunk_count == 2 → verify. Here the chunk
        // count and a naive byte gloss happen to AGREE on the verdict; the row
        // pins that the CHUNK COUNT is what `payload_needs_verify` consults.
        let msg = format!("{}\u{20AC}", "a".repeat(1023)); // € is 3 bytes
        assert_eq!(msg.len(), 1026);
        assert_eq!(
            chunk_text(&msg, CHUNK_BYTES).len(),
            2,
            "the 3-byte char cannot join the 1023B run → a second chunk"
        );
        assert!(
            payload_needs_verify(&msg),
            "two chunks → verify (the chunk count decides)"
        );

        // And the diverging direction: 1022 ASCII + one 3-byte char = 1025 BYTES
        // (>1024 by the byte gloss) but 1022+3 = 1025 > 1024 so it ALSO splits into
        // two chunks. To get a TRUE divergence where byte-count says "verify" yet
        // the chunker says ONE chunk, the multibyte char must FIT: 1021 ASCII + one
        // 3-byte char = 1024 bytes, which is exactly one chunk (1021+3 == 1024) →
        // NO verify, even though it is a multibyte payload over 1023 ASCII bytes.
        let fits = format!("{}\u{20AC}", "a".repeat(1021)); // 1021 + 3 = 1024 bytes
        assert_eq!(fits.len(), 1024);
        assert_eq!(
            chunk_text(&fits, CHUNK_BYTES).len(),
            1,
            "1021 ASCII + a 3-byte char fits one 1024B chunk"
        );
        assert!(
            !payload_needs_verify(&fits),
            "one chunk → no verify (the chunk count decides, not the byte count)"
        );
    }

    // =======================================================================
    // DISPATCH-PTY deferred fresh-child verify (RESPEC-DELTA §3.2) — the S1 fix.
    // =======================================================================

    /// A JSONL transcript with the leading non-user init records a fresh claude
    /// transcript opens with, then the supplied user-text records in order.
    fn build_transcript(user_texts: &[&str]) -> String {
        let mut s = String::new();
        // The exact non-user record kinds observed leading every fresh transcript.
        s.push_str("{\"type\":\"custom-title\",\"title\":\"t\"}\n");
        s.push_str("{\"type\":\"agent-name\",\"name\":\"n\"}\n");
        s.push_str("{\"type\":\"mode\",\"mode\":\"m\"}\n");
        for t in user_texts {
            // content as a bare string (the user_record_text string arm).
            let rec = serde_json::json!({"type":"user","message":{"role":"user","content": t}});
            s.push_str(&serde_json::to_string(&rec).unwrap());
            s.push('\n');
        }
        s
    }

    /// `DeferredVerifyDeps` backed by a REAL on-disk transcript + a virtual clock
    /// (so the real `first_user_text_offset` / `read_user_texts_past_offset`
    /// helpers and the policy are exercised together).
    struct RealFileDeferred {
        path: PathBuf,
        now: Cell<i64>,
    }
    impl DeferredVerifyDeps for RealFileDeferred {
        fn resolve_path(&self) -> Option<PathBuf> {
            self.path.exists().then(|| self.path.clone())
        }
        fn first_user_offset(&self, path: &std::path::Path) -> Option<u64> {
            first_user_text_offset(path)
        }
        fn read_user_texts(
            &self,
            path: &std::path::Path,
            offset: u64,
        ) -> Result<Vec<String>, String> {
            read_user_texts_past_offset(path, offset)
        }
        fn sleep(&self, ms: u64) {
            self.now.set(self.now.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.now.get()
        }
    }

    /// A fully-scripted `DeferredVerifyDeps` (threshold-on-virtual-clock) for the
    /// PENDING/timeout paths — nothing on disk.
    struct ScriptedDeferred {
        now: Cell<i64>,
        resolve_at: i64,
        floor_at: i64,
        texts_at: i64,
        texts: Vec<String>,
    }
    impl DeferredVerifyDeps for ScriptedDeferred {
        fn resolve_path(&self) -> Option<PathBuf> {
            (self.now.get() >= self.resolve_at).then(|| PathBuf::from("/fake/t.jsonl"))
        }
        fn first_user_offset(&self, _path: &std::path::Path) -> Option<u64> {
            (self.now.get() >= self.floor_at).then_some(640)
        }
        fn read_user_texts(
            &self,
            _path: &std::path::Path,
            _offset: u64,
        ) -> Result<Vec<String>, String> {
            if self.now.get() >= self.texts_at {
                Ok(self.texts.clone())
            } else {
                Ok(Vec::new())
            }
        }
        fn sleep(&self, ms: u64) {
            self.now.set(self.now.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.now.get()
        }
    }

    /// A real-file `VerifyDeps` (for the resolvable-path turn in the §7-B mirror):
    /// one read past a fixed offset, then the budget ends.
    struct FileVerifyDeps {
        path: PathBuf,
        offset: u64,
        now: Cell<i64>,
    }
    impl VerifyDeps for FileVerifyDeps {
        fn read_user_texts(&self) -> Result<Vec<String>, String> {
            read_user_texts_past_offset(&self.path, self.offset)
        }
        fn sleep(&self, _ms: u64) {}
        fn now_ms(&self) -> i64 {
            let v = self.now.get();
            self.now.set(v + 1_000_000);
            v
        }
    }

    #[test]
    fn first_user_offset_skips_init_records_never_byte_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let content = build_transcript(&["hello turn one"]);
        std::fs::write(&path, &content).unwrap();

        let off = first_user_text_offset(&path).expect("a user record is present");
        // NIT-2: never byte 0 — the floor is past the leading init records.
        assert!(off > 0, "floor must skip the init records, got {off}");
        // The floor anchors exactly at the first user record, so a read past it
        // returns only that user turn (not the init records).
        let texts = read_user_texts_past_offset(&path, off).unwrap();
        assert_eq!(texts, vec!["hello turn one".to_string()]);
    }

    #[test]
    fn first_user_offset_none_when_only_init_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        // Init records only — no user turn has landed yet (the caller keeps polling).
        std::fs::write(&path, build_transcript(&[])).unwrap();
        assert_eq!(first_user_text_offset(&path), None);
    }

    #[test]
    fn deferred_verify_fires_for_resolved_fresh_child() {
        // The S1 fix: once the fresh child's transcript resolves WITH turn-1's
        // record past the high-water floor, the deferred verify returns Verified
        // (→ the caller emits the pty `message-seen` that was previously skipped).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, build_transcript(&["the priming turn"])).unwrap();

        let deps = RealFileDeferred {
            path,
            now: Cell::new(0),
        };
        let out = deferred_verify_fresh_child(
            &deps,
            "the priming turn",
            VERIFY_TIMEOUT_S,
            VERIFY_POLL_MS,
        );
        assert_eq!(out, PayloadVerifyOutcome::Verified);
    }

    #[test]
    fn deferred_verify_pending_when_transcript_never_resolves() {
        // Anti-phantom: a send whose transcript never appears emits NOTHING — the
        // outcome is a visible PENDING stall, never a (wrong) message-seen.
        let deps = ScriptedDeferred {
            now: Cell::new(0),
            resolve_at: i64::MAX, // never resolves within the budget
            floor_at: 0,
            texts_at: 0,
            texts: vec!["x".into()],
        };
        let out = deferred_verify_fresh_child(&deps, "x", VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert!(
            matches!(out, PayloadVerifyOutcome::SourceUnavailable(_)),
            "never-resolved → SourceUnavailable (PENDING), got {out:?}"
        );
    }

    #[test]
    fn deferred_verify_pending_when_turn_never_lands() {
        // The transcript + a user record resolve, but OUR content never appears
        // past the floor in the window → PENDING (no false-fire).
        let deps = ScriptedDeferred {
            now: Cell::new(0),
            resolve_at: 1_000,
            floor_at: 1_000,
            texts_at: i64::MAX, // the matching text never lands
            texts: vec!["our message".into()],
        };
        let out =
            deferred_verify_fresh_child(&deps, "our message", VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(
            out,
            PayloadVerifyOutcome::NoRecord,
            "floor seen but content never matched → NoRecord (PENDING)"
        );
    }

    #[test]
    fn deferred_verify_refuses_ambiguous_identical_bodies() {
        // NIT-2 binding: ≥2 identical user records past the floor is an AMBIGUITY —
        // refuse to PENDING, NEVER anchor on the earlier one. (This shape cannot
        // arise for a genuinely fresh child; it guards the transiently-unresolvable
        // EXISTING-session edge.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, build_transcript(&["DUP", "DUP"])).unwrap();
        let deps = RealFileDeferred {
            path,
            now: Cell::new(0),
        };
        let out = deferred_verify_fresh_child(&deps, "DUP", VERIFY_TIMEOUT_S, VERIFY_POLL_MS);
        assert_eq!(
            out,
            PayloadVerifyOutcome::Unattributable,
            "two identical bodies past the floor must refuse (PENDING), never fire"
        );
    }

    #[test]
    fn identical_body_turns_each_anchor_own_record_no_reanchor() {
        // §7-B MIRROR (unit level): two identical-body priming turns each fire
        // their OWN seen — the second off the SECOND record, never re-anchoring on
        // the first identical body.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");

        // PHASE 1 — turn-1 (the DEFERRED fresh-child path): transcript = [init][DUP#1].
        let v1 = build_transcript(&["DUP"]);
        std::fs::write(&path, &v1).unwrap();
        let deps1 = RealFileDeferred {
            path: path.clone(),
            now: Cell::new(0),
        };
        assert_eq!(
            deferred_verify_fresh_child(&deps1, "DUP", VERIFY_TIMEOUT_S, VERIFY_POLL_MS),
            PayloadVerifyOutcome::Verified,
            "turn-1 anchors on its own (the first) DUP record"
        );

        // PHASE 2 — turn-2 (the RESOLVABLE path, gated on-received AFTER turn-1's
        // seen): the transcript now carries DUP#2; turn-2's pre-send offset is the
        // byte boundary AFTER DUP#1's record.
        let pre_send_offset = v1.len() as u64;
        let v2 = format!(
            "{v1}{}\n",
            serde_json::to_string(
                &serde_json::json!({"type":"user","message":{"role":"user","content":"DUP"}})
            )
            .unwrap()
        );
        std::fs::write(&path, &v2).unwrap();

        // The high-water offset EXCLUDES DUP#1 — a read past it sees only DUP#2,
        // so an exact-text scan can only anchor on the SECOND record.
        let past = read_user_texts_past_offset(&path, pre_send_offset).unwrap();
        assert_eq!(
            past,
            vec!["DUP".to_string()],
            "exactly one DUP past the offset (the second)"
        );

        // And verify_chunked_payload over that offset confirms turn-2 (no re-anchor).
        let vdeps = FileVerifyDeps {
            path: path.clone(),
            offset: pre_send_offset,
            now: Cell::new(0),
        };
        assert_eq!(
            verify_chunked_payload(&vdeps, "DUP", VERIFY_TIMEOUT_S, VERIFY_POLL_MS),
            PayloadVerifyOutcome::Verified,
            "turn-2 verifies against the second DUP record"
        );
    }

    // =======================================================================
    // §7-B BUSY-QUEUED deferred verify (PTY-FIX-DESIGN.md §4) — the RE-SCAN fix.
    // The busy-queued floor is the CAPTURED PRE-SEND OFFSET (constraint 1), not
    // the first user-text record; otherwise the policy is shared byte-for-byte
    // with the fresh-child path via `deferred_verify_floor_core`.
    // =======================================================================

    /// `DeferredVerifyDeps` backed by a REAL on-disk transcript + a virtual clock,
    /// mirroring the PRODUCTION `DeferredBusyQueuedDeps` (send.rs): an EXISTING
    /// session whose transcript is already resolved (`resolve_path` hands it back)
    /// and whose high-water floor is the captured pre-send offset (`first_user_offset`
    /// returns the fixed `floor`, ignoring the path).
    struct RealFileBusyQueued {
        path: PathBuf,
        floor: u64,
        now: Cell<i64>,
    }
    impl DeferredVerifyDeps for RealFileBusyQueued {
        fn resolve_path(&self) -> Option<PathBuf> {
            Some(self.path.clone())
        }
        fn first_user_offset(&self, _path: &std::path::Path) -> Option<u64> {
            Some(self.floor)
        }
        fn read_user_texts(
            &self,
            path: &std::path::Path,
            offset: u64,
        ) -> Result<Vec<String>, String> {
            read_user_texts_past_offset(path, offset)
        }
        fn sleep(&self, ms: u64) {
            self.now.set(self.now.get() + ms as i64);
        }
        fn now_ms(&self) -> i64 {
            self.now.get()
        }
    }

    #[test]
    fn busy_queued_fires_when_queued_turn_lands_past_floor() {
        // The §7-B fix: a busy-queued send whose queued turn lands as a user record
        // PAST the captured pre-send floor returns Verified (→ the caller emits the
        // pty `message-seen` that was previously STRUCTURALLY impossible on this
        // path). Transcript = [init][prior in-flight turn][the queued turn]; the
        // floor is the byte boundary after the prior turn (the pre-send snapshot).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let v1 = build_transcript(&["the prior in-flight turn"]);
        let floor = v1.len() as u64; // pre-send offset, snapshotted before typing
        let queued = serde_json::to_string(
            &serde_json::json!({"type":"user","message":{"role":"user","content":"the queued turn"}}),
        )
        .unwrap();
        std::fs::write(&path, format!("{v1}{queued}\n")).unwrap();

        let deps = RealFileBusyQueued {
            path,
            floor,
            now: Cell::new(0),
        };
        assert_eq!(
            deferred_verify_busy_queued(
                &deps,
                "the queued turn",
                BUSY_QUEUED_VERIFY_TIMEOUT_S,
                VERIFY_POLL_MS
            ),
            PayloadVerifyOutcome::Verified,
            "the queued turn landing past the floor → Verified (emit message-seen)"
        );
    }

    #[test]
    fn busy_queued_floor_excludes_prior_identical_turn_no_reanchor() {
        // §7-B ANTI-PHANTOM (the wrong-fire risk, NIT-2 class): the prior in-flight
        // turn and the queued turn carry the IDENTICAL body. The high-water floor
        // sits PAST the prior turn's record, so an exact-text re-scan sees ONLY the
        // queued turn's record → exactly one match → Verified, anchored on THIS
        // turn, NEVER re-anchored on the earlier identical body.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let v1 = build_transcript(&["DUP"]); // [init][DUP#1] (the prior in-flight turn)
        let floor = v1.len() as u64;
        let dup2 = serde_json::to_string(
            &serde_json::json!({"type":"user","message":{"role":"user","content":"DUP"}}),
        )
        .unwrap();
        std::fs::write(&path, format!("{v1}{dup2}\n")).unwrap();

        // The floor EXCLUDES DUP#1 — a read past it sees only DUP#2.
        assert_eq!(
            read_user_texts_past_offset(&path, floor).unwrap(),
            vec!["DUP".to_string()],
            "exactly one DUP past the pre-send floor (the queued turn, not the prior)"
        );
        let deps = RealFileBusyQueued {
            path,
            floor,
            now: Cell::new(0),
        };
        assert_eq!(
            deferred_verify_busy_queued(&deps, "DUP", BUSY_QUEUED_VERIFY_TIMEOUT_S, VERIFY_POLL_MS),
            PayloadVerifyOutcome::Verified,
            "identical prior body before the floor must NOT cause a re-anchor — Verified on the queued turn"
        );
    }

    #[test]
    fn busy_queued_refuses_two_identical_bodies_past_floor() {
        // NIT-2 binding (constraint 3): ≥2 identical bodies PAST the floor (e.g. a
        // relay→pty fallback double-delivery) is an AMBIGUITY — REFUSE to PENDING
        // (Unattributable), NEVER anchor on either. A short timeout keeps the
        // budget-exhaustion path snappy.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let v1 = build_transcript(&["the prior in-flight turn"]);
        let floor = v1.len() as u64;
        let dup = serde_json::to_string(
            &serde_json::json!({"type":"user","message":{"role":"user","content":"DUP"}}),
        )
        .unwrap();
        std::fs::write(&path, format!("{v1}{dup}\n{dup}\n")).unwrap();

        let deps = RealFileBusyQueued {
            path,
            floor,
            now: Cell::new(0),
        };
        assert_eq!(
            deferred_verify_busy_queued(&deps, "DUP", 2, VERIFY_POLL_MS),
            PayloadVerifyOutcome::Unattributable,
            "two identical bodies past the floor must refuse (PENDING), never fire"
        );
    }

    #[test]
    fn busy_queued_pending_when_queued_turn_never_lands() {
        // The transcript + the floor resolve, but the queued turn's record never
        // appears past the floor in the bounded window → NoRecord → the caller
        // degrades to a VISIBLE PENDING (no emit, no false-fire). Bounded budget →
        // never a hang (the virtual clock advances on sleep).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let v1 = build_transcript(&["the prior in-flight turn"]);
        let floor = v1.len() as u64;
        std::fs::write(&path, &v1).unwrap(); // nothing past the floor ever lands

        let deps = RealFileBusyQueued {
            path,
            floor,
            now: Cell::new(0),
        };
        assert_eq!(
            deferred_verify_busy_queued(&deps, "the queued turn", 2, VERIFY_POLL_MS),
            PayloadVerifyOutcome::NoRecord,
            "floor seen but the queued turn never matched → NoRecord (PENDING, no emit)"
        );
    }
}
