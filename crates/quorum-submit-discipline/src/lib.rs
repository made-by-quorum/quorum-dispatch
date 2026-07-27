//! Pure submit discipline (submit-extract LEAF crate).
//!
//! The acceptance-keyed verify-then-CR core, the chunked two-write PTY delivery
//! discipline, the bounded-retry `deliver_prompt` wrapper, and the
//! content-verified-CR composer predicate — all trait-abstracted with ZERO
//! dependency on `dispatch` or `qrmux`. Extracted VERBATIM from
//! `dispatch::submit` / `dispatch::sendpty` so both crates can drive the same
//! discipline without a `qrmux → dispatch` cycle; the concrete `Real*`
//! (Mux/Clock/fs) bindings and the W8 transcript-verify / `--wait` layer STAY in
//! dispatch (they are coupled to `crate::sendpty`'s JSONL parsing) and re-export
//! every public name moved here.
//!
//! ---------------------------------------------------------------------------
//! ORIGINAL file-header war-story (from `dispatch::submit`, itself an EXACT port
//! of `qa/hardening@3dd9f1e:src/commands/submit.ts:1-26`; TS-internal codename
//! redacted per engine scope-audit — RATIONALE fidelity, not name-byte fidelity,
//! per the comments-carry ruling):
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

use std::path::PathBuf;

// ==========================================================================
// PURE SUBMIT DISCIPLINE (moved verbatim from dispatch::submit lines 56-625).
// ==========================================================================
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
    /// Which composer region the content-verify anchors on (M4). Defaults to the
    /// claude/❯ glyph anchor via [`ContentVerifiedSubmit::new`]; the per-harness
    /// fire passes codex/pi regions via [`ContentVerifiedSubmit::new_in_region`].
    region: ComposerRegion,
}

impl<D: IdleDeliverDeps> ContentVerifiedSubmit<D> {
    /// Wrap `inner` so its remediation CR is content-verified against `message`,
    /// anchored on the default claude/❯ glyph region.
    pub fn new(inner: D, message: &str) -> Self {
        Self::new_in_region(inner, message, ComposerRegion::GlyphAnchor(PROMPT_GLYPH))
    }

    /// As [`ContentVerifiedSubmit::new`], but with an explicit composer region
    /// (M4: codex `GlyphAnchor('›')`, pi `BetweenLastTwo('─')`).
    pub fn new_in_region(inner: D, message: &str, region: ComposerRegion) -> Self {
        Self {
            inner,
            message: message.to_string(),
            region,
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
        // genuinely visible + stuck). Anchored on the per-harness composer region
        // (claude/codex glyph, pi two-rule) so a scrollback echo can't
        // false-positive.
        if composer_holds_message_in(&self.inner.read_screen(), &self.message, self.region) {
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

/// As [`deliver_idle_two_write`], but the content-verified remediation CR anchors
/// on an explicit [`ComposerRegion`] (M4: the per-harness fire passes the codex/pi
/// region). The two-write + chunking + acceptance machinery is identical; only the
/// region the remediation CR checks changes.
pub fn deliver_idle_two_write_in_region(
    deps: &dyn IdleDeliverDeps,
    message: &str,
    opts: SubmitOptions,
    region: ComposerRegion,
) -> SubmitOutcome {
    deliver_idle_two_write_with_region(deps, message, opts, ChunkSendOptions::default(), region)
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
    deliver_idle_two_write_with_region(
        deps,
        message,
        opts,
        chunk_opts,
        ComposerRegion::GlyphAnchor(PROMPT_GLYPH),
    )
}

/// The full form: [`deliver_idle_two_write_with`] plus an explicit
/// [`ComposerRegion`] for the content-verified remediation CR (M4). The default
/// callers pin `GlyphAnchor(PROMPT_GLYPH)` — byte-for-byte the prior behaviour.
pub fn deliver_idle_two_write_with_region(
    deps: &dyn IdleDeliverDeps,
    message: &str,
    opts: SubmitOptions,
    chunk_opts: ChunkSendOptions,
    region: ComposerRegion,
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
    let verified = ContentVerifiedSubmit::new_in_region(deps, message, region);
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
/// external spawn caller, so it stays distinct.
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
/// prompts (an external caller's spawn) are the LIKELIEST ≥4KB case and share the exact
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

// ==========================================================================
// CONTENT-VERIFIED-CR COMPOSER PREDICATE (moved verbatim from
// dispatch::sendpty lines 170-237).
// ==========================================================================
// --- send:pty queued-send stuck detection (content-verified CR) ------------
//
// VERBATIM (qa/hardening@3dd9f1e:src/utils.ts:367-378):
// A busy-session queued send delivers as two writes (text, then "\r" alone) to
// mimic a human keystroke. If even that leaves the message unsubmitted in the
// composer (paste-burst still absorbing the \r), we remediate — but ONLY when we
// can SEE our text sitting unsubmitted. This is the predicate for that decision:
// does the session's screen still hold OUR message in the composer?
//
// Why this is safe under the revised contract (see commands/send.ts): we never
// BLIND-CR a busy session — every remediation CR is conditional on this returning
// true, i.e. on our exact text being visibly present and unsubmitted, so the CR
// can only submit OUR message. If it already queued, the composer is empty, this
// returns false, and no CR is emitted.

/// The live composer prompt glyph (`❯`) — the region after the LAST one is the
/// composer (scrollback quotes appear before it).
pub const PROMPT_GLYPH: char = '\u{276f}'; // ❯

/// How to locate the live-composer region within a rendered screen dump for the
/// content-verified CR (M4 generalization of the ❯-only anchor). A glyph-anchored
/// harness rides [`ComposerRegion::GlyphAnchor`] (claude `❯`, codex `›`); a
/// glyph-less harness whose composer is framed by horizontal rules rides
/// [`ComposerRegion::BetweenLastTwo`] (pi `─`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerRegion {
    /// The region AFTER the LAST occurrence of this glyph char (the live prompt;
    /// a processed turn echoed in scrollback appears BEFORE it, so it can never
    /// false-positive as "still unsent"). No glyph present → the WHOLE dump
    /// (conservative: a false "stuck" costs one extra content-verified CR, never a
    /// blind one). This is the claude/`❯` (default) and codex/`›` semantics.
    GlyphAnchor(char),
    /// The region strictly BETWEEN the LAST TWO full-width rule lines built from
    /// this char (pi's composer sits between two `─` U+2500 rules; the footer is
    /// below the bottom rule, scrollback above the top rule). Fewer than two rule
    /// lines (the composer scrolled so the top rule is off-screen, or a modal
    /// overlay dropped a rule) → NO locatable region → the predicate is FALSE
    /// (no CR → honest not-accepted + re-show). It deliberately does NOT fall back
    /// to the whole dump: on a glyph-less harness the whole dump matches BOTH the
    /// live composer AND the post-submit scrollback echo (unsent-vs-landed becomes
    /// indistinguishable), so the safe direction is "cannot confirm it holds".
    BetweenLastTwo(char),
}

/// A "rule line": a non-empty line whose non-whitespace chars are ALL `rule` and
/// which holds a run of at least this many of them — a full-width horizontal rule
/// (pi's ~120-col `─`), not a stray box-drawing char inside composer content.
///
/// N2 (M4 red-team r1, SAFE-DIRECTION, deferred): a payload/draft line that is
/// ITSELF ≥`MIN_RULE_RUN` bare `rule` chars is mis-counted as a frame rule, so
/// `between_last_two_rules` mis-bounds the region. The effect is safe-direction
/// ONLY — the region collapses/moves ⇒ `composer_holds_message_in` returns FALSE
/// ⇒ the guarded remediation CR is suppressed (honest not-accepted), NEVER a false
/// "holds"/false-landed. Left as-is rather than complicating the rule (frame width
/// is unknown at this layer, so a higher threshold would risk narrow terminals and
/// regress the common case). Re-entry: M5 real-drive, if a real payload ever hits it.
const MIN_RULE_RUN: usize = 3;

fn is_rule_line(line: &str, rule: char) -> bool {
    let mut count = 0usize;
    for ch in line.chars() {
        if ch == rule {
            count += 1;
        } else if !ch.is_whitespace() {
            return false; // a non-rule, non-ws char ⇒ composer content, not a rule
        }
    }
    count >= MIN_RULE_RUN
}

/// The region strictly BETWEEN the last two rule lines of an ANSI-stripped dump
/// (pi's composer). `None` if fewer than two rule lines are present. Byte offsets
/// land on `\n`/line boundaries (ASCII), so the returned slice is always valid.
fn between_last_two_rules(clean: &str, rule: char) -> Option<&str> {
    let mut rules: Vec<(usize, usize)> = Vec::new(); // (line_start, line_end) byte offsets
    let mut pos = 0usize;
    for line in clean.split('\n') {
        let start = pos;
        let end = start + line.len();
        if is_rule_line(line, rule) {
            rules.push((start, end));
        }
        pos = end + 1; // step past the '\n' separator
    }
    if rules.len() < 2 {
        return None;
    }
    let top = rules[rules.len() - 2];
    let bottom = rules[rules.len() - 1];
    let region_start = top.1.min(clean.len()); // just after the top rule line
    let region_end = bottom.0.min(clean.len()); // just before the bottom rule line
    (region_start <= region_end).then(|| &clean[region_start..region_end])
}

/// Extract the live-composer region of an ALREADY-ANSI-STRIPPED dump per `region`.
/// `None` ⇒ no locatable region (the `BetweenLastTwo` < 2-rules case). The
/// `GlyphAnchor` arm never returns `None` (whole-dump fallback).
fn region_of<'a>(clean: &'a str, region: ComposerRegion) -> Option<&'a str> {
    match region {
        ComposerRegion::GlyphAnchor(g) => Some(match clean.rfind(g) {
            Some(idx) => &clean[idx..],
            None => clean,
        }),
        ComposerRegion::BetweenLastTwo(rule) => between_last_two_rules(clean, rule),
    }
}

/// Collapse ALL whitespace runs to single spaces and trim — so a composer that
/// WRAPS a long message across lines still matches the one-line needle. Port of
/// `normalizeWs` (qa/hardening@3dd9f1e:src/utils.ts:387-389,
/// `s.replace(/\s+/g, " ").trim()`).
pub fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

/// Does the session's screen still hold OUR message unsubmitted in the composer?
/// Port of `composerHoldsMessage` (qa/hardening@3dd9f1e:src/utils.ts:391-423).
///
/// ---------------------------------------------------------------------------
/// VERBATIM (utils.ts:391-405 doc-comment):
/// Anchored to the region AFTER the LAST prompt glyph (❯) in the dump: that glyph
/// marks the live composer prompt, so a processed user turn quoted in scrollback
/// (which appears BEFORE the last ❯) can never false-positive as "still unsent".
/// If no glyph is present we fall back to scanning the whole dump (conservative:
/// a false "stuck" only costs one extra content-verified CR, never a blind one).
///
/// Both haystack and needle are ANSI-stripped and whitespace-normalized, so a
/// message the composer wrapped across several lines still matches.
/// ---------------------------------------------------------------------------
pub fn composer_holds_message(screen_text: &str, message: &str) -> bool {
    // The default claude/❯ anchor: byte-for-byte the prior behaviour (glyph-anchor
    // with whole-dump fallback), now expressed via the generalized region form.
    composer_holds_message_in(
        screen_text,
        message,
        ComposerRegion::GlyphAnchor(PROMPT_GLYPH),
    )
}

/// As [`composer_holds_message`], but with an explicit [`ComposerRegion`] (M4):
/// codex rides `GlyphAnchor('›')`; pi rides `BetweenLastTwo('─')`. Both haystack
/// (the located region) and needle are ANSI-stripped and whitespace-normalized, so
/// a message the composer wrapped across several lines still matches (QS-2). A
/// region that cannot be located (`BetweenLastTwo` with < 2 rules) is `false` —
/// never a blind claim that the composer holds our text.
pub fn composer_holds_message_in(
    screen_text: &str,
    message: &str,
    region: ComposerRegion,
) -> bool {
    let needle = normalize_ws(&strip_ansi(message));
    if needle.is_empty() {
        return false;
    }
    let clean = strip_ansi(screen_text);
    match region_of(&clean, region) {
        Some(r) => normalize_ws(r).contains(&needle),
        None => false,
    }
}

// ==========================================================================
// CARRIED (copy, not reference) from dispatch::boot::strip_ansi — a pure,
// std-only ANSI/terminal-control stripper. composer_holds_message above needs
// it, and this LEAF crate cannot depend on dispatch, so the small pure fn is
// duplicated here (dispatch keeps its own boot::strip_ansi for other callers).
// Byte-identical to boot.rs so the W6 differential test below still holds.
// ==========================================================================
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC. Look at the next byte to classify the sequence.
            let Some(&next) = bytes.get(i + 1) else {
                // Lone trailing ESC — drop it.
                break;
            };
            // Compute the byte just past the escape sequence, then SNAP to a
            // char boundary: `zmx history` is external/possibly-truncated, and a
            // multibyte char immediately after a (malformed) escape would
            // otherwise leave `i` mid-char and panic the slice below (L8: never
            // panic on junk from an external tool — found via adversarial probe
            // `strip_ansi("\x1b(中")`).
            let skip_to = match next {
                b'[' => {
                    // CSI: ESC [ <params/intermediates> <final 0x40..=0x7e>.
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    // Skip the final byte too (j points at it, if present).
                    if j < bytes.len() {
                        j + 1
                    } else {
                        j
                    }
                }
                b']' => {
                    // OSC: ESC ] ... terminated by BEL (0x07) or ST (ESC \).
                    let mut j = i + 2;
                    loop {
                        if j >= bytes.len() {
                            break;
                        }
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    j
                }
                b'(' | b')' | b'*' | b'+' => {
                    // Charset designation: ESC ( B  etc. — ESC + intermediate +
                    // one final byte. Skip all three (or two if truncated).
                    i + 3
                }
                _ => {
                    // Any other two-byte escape (ESC = / ESC > / ESC M …): skip
                    // ESC + the one following byte.
                    i + 2
                }
            };
            i = floor_to_char_boundary(input, skip_to.min(bytes.len()));
            continue;
        }
        // Drop bare control bytes (CR, BEL, …) but keep newline + tab so the
        // tail's line structure survives for the marker scan.
        if b == b'\n' || b == b'\t' {
            out.push(b as char);
            i += 1;
            continue;
        }
        if b < 0x20 || b == 0x7f {
            i += 1;
            continue;
        }
        // Copy ONE whole UTF-8 char starting at the (boundary-aligned) `i`. Using
        // `chars().next()` instead of a hand-computed length keeps the slice
        // char-boundary-safe even if `i` somehow points at a continuation byte.
        match input[i..].chars().next() {
            Some(ch) => {
                out.push(ch);
                i += ch.len_utf8();
            }
            None => i += 1,
        }
    }
    out
}

/// Advance `idx` forward to the nearest UTF-8 char boundary at or after it
/// (`str::ceil_char_boundary` is unstable, so this is the hand-rolled form). A
/// computed escape-skip index that lands inside a multibyte char is moved to the
/// start of the NEXT char so the subsequent slice never panics.
fn floor_to_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    // --- moved from dispatch::submit tests (pure submit-discipline rows) ---
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

    // --- moved from dispatch::sendpty tests (normalize_ws / composer / W6) -
    // --- normalize_ws -----------------------------------------------------

    #[test]
    fn normalize_ws_collapses_and_trims() {
        assert_eq!(normalize_ws("  a \n\t b   c \n"), "a b c");
        assert_eq!(normalize_ws(""), "");
        assert_eq!(normalize_ws("   "), "");
        assert_eq!(normalize_ws("solid"), "solid");
    }

    // --- composer_holds_message (ported + additions) ----------------------

    const ESC: char = '\u{1b}';

    /// A screen dump: scrollback, then the live composer prompt line (TS test
    /// helper `screen`).
    fn screen(composer: &str, scrollback: &str) -> String {
        format!("{scrollback}\n\u{276f} {composer}\n")
    }

    #[test]
    fn composer_exact_match() {
        assert!(composer_holds_message(
            &screen("SMOKE4: hello there", ""),
            "SMOKE4: hello there"
        ));
    }

    #[test]
    fn composer_wrapped_across_lines_matches() {
        // The composer wraps a long message; whitespace-normalization collapses
        // the injected newlines so the one-line needle still matches.
        let wrapped = "this is a very long\nmessage that the composer\nwrapped across lines";
        assert!(composer_holds_message(
            &screen(wrapped, ""),
            "this is a very long message that the composer wrapped across lines"
        ));
    }

    #[test]
    fn composer_ansi_decorated_matches_after_strip() {
        let decorated = format!("{ESC}[2m{ESC}[36mqueued text{ESC}[0m");
        assert!(composer_holds_message(
            &screen(&decorated, ""),
            "queued text"
        ));
    }

    #[test]
    fn composer_absent_empty_composer_is_false() {
        assert!(!composer_holds_message(
            &screen("", ""),
            "SMOKE4: hello there"
        ));
    }

    #[test]
    fn composer_processed_turn_in_scrollback_does_not_false_positive() {
        // The same text appears as an ALREADY-PROCESSED turn (before the last ❯),
        // and the composer is now empty. Anchoring to the region after the LAST ❯
        // excludes scrollback, so this is correctly NOT stuck.
        let scrollback = "\u{276f} SMOKE4: hello there\n  (assistant replied...)";
        assert!(!composer_holds_message(
            &screen("", scrollback),
            "SMOKE4: hello there"
        ));
    }

    #[test]
    fn composer_empty_needle_is_false() {
        assert!(!composer_holds_message(&screen("", ""), ""));
        // A needle that is only whitespace also normalizes to empty → false.
        assert!(!composer_holds_message(&screen("anything", ""), "   "));
    }

    #[test]
    fn composer_no_glyph_falls_back_to_whole_dump() {
        // No ❯ anywhere → scan the whole dump (conservative). Our text present →
        // true.
        assert!(composer_holds_message(
            "plain dump with queued text in it",
            "queued text"
        ));
        assert!(!composer_holds_message(
            "plain dump without it",
            "queued text"
        ));
    }

    #[test]
    fn composer_unicode_message_matches() {
        // a4-spec D: Unicode. A multibyte message survives strip+normalize and
        // matches in the composer region.
        let msg = "café ☕ 日本語 — naïve";
        assert!(composer_holds_message(&screen(msg, ""), msg));
        // And it does NOT match when only present in scrollback (empty composer).
        let scrollback = format!("\u{276f} {msg}\n (replied)");
        assert!(!composer_holds_message(&screen("", &scrollback), msg));
    }

    // --- ComposerRegion generalization (M4: codex glyph, pi two-rule) ------

    const CODEX_GLYPH: char = '\u{203a}'; // ›
    const PI_RULE: char = '\u{2500}'; // ─

    /// The default `composer_holds_message` is EXACTLY `GlyphAnchor(❯)` — same
    /// bytes in, same bool out. (Byte-for-byte regression guard for the claude
    /// default path M1 is accepted on.)
    #[test]
    fn default_equals_glyph_anchor_prompt_glyph() {
        for (composer, sb, needle) in [
            ("SMOKE4: hello there", "", "SMOKE4: hello there"),
            ("", "\u{276f} echoed\n(reply)", "echoed"),
            ("café ☕", "", "café ☕"),
        ] {
            let s = screen(composer, sb);
            assert_eq!(
                composer_holds_message(&s, needle),
                composer_holds_message_in(&s, needle, ComposerRegion::GlyphAnchor(PROMPT_GLYPH)),
                "default must equal GlyphAnchor(❯) for ({composer:?},{sb:?},{needle:?})"
            );
        }
    }

    /// codex: the region is anchored after the LAST `›` (U+203A). A processed turn
    /// echoed in scrollback (with its own `›`) sits before the live composer's `›`,
    /// so it never false-positives — exact claude-equivalent semantics on the codex
    /// glyph.
    #[test]
    fn codex_glyph_anchor_region() {
        let region = ComposerRegion::GlyphAnchor(CODEX_GLYPH);
        // Live composer holds our text (after the last ›).
        let live = format!("{CODEX_GLYPH} Reply with exactly: q7-ok");
        assert!(composer_holds_message_in(&live, "Reply with exactly: q7-ok", region));
        // Echoed in scrollback BEFORE the last (empty) composer glyph → NOT held.
        // (Empty codex composers carry ghost placeholder text, modelled here.)
        let echoed = format!(
            "{CODEX_GLYPH} Reply with exactly: q7-ok\n• q7-ok\n{CODEX_GLYPH} Summarize recent commits"
        );
        assert!(!composer_holds_message_in(&echoed, "Reply with exactly: q7-ok", region));
        // The ASCII banner `>_` (U+003E) must not collide with U+203A anchoring.
        let with_banner = format!("│ >_ OpenAI Codex (v0.144.1) │\n{CODEX_GLYPH} hi there");
        assert!(composer_holds_message_in(&with_banner, "hi there", region));
    }

    /// A pi-shaped screen: scrollback, a full-width top rule, the composer, a
    /// full-width bottom rule, then the footer.
    fn pi_screen(composer: &str, scrollback: &str) -> String {
        let rule: String = std::iter::repeat(PI_RULE).take(60).collect();
        format!(
            "{scrollback}\n{rule}\n{composer}\n{rule}\n~/work (branch)  $0.000 gpt-5.5 • medium\n"
        )
    }

    /// pi: the region is BETWEEN the last two `─` rules. The bare scrollback echo
    /// (no glyph, above the top rule) and the footer (below the bottom rule) are
    /// both OUTSIDE the region, so neither false-positives.
    #[test]
    fn pi_between_two_rules_region() {
        let region = ComposerRegion::BetweenLastTwo(PI_RULE);
        // Composer holds our text.
        assert!(composer_holds_message_in(
            &pi_screen("q7 probe pi alpha", ""),
            "q7 probe pi alpha",
            region
        ));
        // Same text ONLY as a bare scrollback echo, composer empty → NOT held
        // (the whole-dump fallback WOULD false-positive here; the region rule does
        // not). This is the unsent-vs-landed distinction the glyph-swap cannot make.
        assert!(!composer_holds_message_in(
            &pi_screen("", " q7 probe pi alpha"),
            "q7 probe pi alpha",
            region
        ));
        // A message the composer wrapped across lines still matches (QS-2).
        assert!(composer_holds_message_in(
            &pi_screen("a very long pi\nmessage wrapped\nacross columns", ""),
            "a very long pi message wrapped across columns",
            region
        ));
    }

    /// pi safe floor: fewer than two rules (top rule scrolled off / a modal dropped
    /// a rule) → NO locatable region → FALSE (no CR → honest not-accepted), NEVER
    /// the ambiguous whole-dump fallback.
    #[test]
    fn pi_fewer_than_two_rules_is_false_not_whole_dump() {
        let region = ComposerRegion::BetweenLastTwo(PI_RULE);
        let one_rule: String = std::iter::repeat(PI_RULE).take(60).collect();
        // Only ONE rule visible; our text is on screen but the region can't be
        // bounded → false (would be a whole-dump true if we fell back).
        let scrolled = format!("q7 probe pi alpha\n{one_rule}\nfooter");
        assert!(!composer_holds_message_in(&scrolled, "q7 probe pi alpha", region));
        // Zero rules → false.
        assert!(!composer_holds_message_in("q7 probe pi alpha only", "q7 probe pi alpha only", region));
        // A stray short ─ run inside content is NOT a rule line (< MIN_RULE_RUN).
        let stray = format!("a ─ b\n{one_rule}\nx");
        assert!(!composer_holds_message_in(&stray, "a ─ b", region));
    }

    // --- W6 differential: boot::strip_ansi vs TS stripAnsi semantics ------
    //
    // a4-spec §2.3 W6 (MANDATORY): before adopting boot::strip_ansi as the
    // composer stripper, run it against the TS stripAnsi regex semantics
    // (utils.ts:373-383) on the input classes the regex targets — OSC title
    // sequences (ESC ] 0;… BEL and ESC ] …ST), CSI color/cursor runs, and lone
    // ESC. `ts_strip_ansi_reference` is a faithful port of the THREE TS regexes,
    // used as the oracle.
    //
    // RESULT (flagged to the lead — do NOT "resolve" this silently): boot and TS
    // AGREE on every CSI and OSC class (the only escapes claude's TUI emits in a
    // composer dump — verified by the boot-corpus tests + the cases below). They
    // DIVERGE on the bare lone-ESC class: TS's `\x1b[@-_]?` consumes the next byte
    // ONLY when it is in 0x40..=0x5f, whereas boot's catch-all 2-byte rule
    // (boot.rs:215-219) consumes ESC + ANY following byte. So
    // `strip_ansi("plain\x1b=more")` is boot→"plainmore" vs TS→"plain=more"
    // (`=` is 0x3d), and `"\x1b xtext"`-style sequences differ likewise.
    //
    // We do NOT extend boot::strip_ansi to the narrower TS rule: boot's existing
    // `strip_ansi_charset_keypad_and_lone_esc` test (boot.rs:693-697) ASSERTS the
    // wider behavior (it strips `\x1b=` whole — keypad-mode is real claude TUI
    // output), so narrowing would regress a sanctioned boot invariant. The two
    // strippers serve different corpora with a deliberate, now-DOCUMENTED edge
    // difference that is inert for the composer (a composer dump carries no bare
    // `ESC <0x20-0x3f/0x60-0x7e>` runs — those bytes only appear inside the CSI/OSC
    // payloads both strip identically). The `w6_*` tests below pin BOTH the
    // agreement (CSI/OSC) and the documented divergence (lone-ESC) so a future
    // change to either stripper is caught. ESCALATION: this is a flagged semantic
    // edge for the lead, not a silent adoption.

    /// Reference port of the TS `stripAnsi` regexes (utils.ts:373-383), applied in
    /// the same order:
    ///   1. CSI:  \x1b\[[0-9;?]*[ -/]*[@-~]
    ///   2. OSC:  \x1b\][^\x07\x1b]*(?:\x07|\x1b\\)
    ///   3. lone: \x1b[@-_]?
    ///
    /// Hand-rolled (no regex dep) but faithful to the regex semantics.
    fn ts_strip_ansi_reference(s: &str) -> String {
        let b = s.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == 0x1b {
                // Try CSI: ESC [ [0-9;?]* [ -/]* [@-~]
                if b.get(i + 1) == Some(&b'[') {
                    let mut j = i + 2;
                    while j < b.len() && matches!(b[j], b'0'..=b'9' | b';' | b'?') {
                        j += 1;
                    }
                    while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                        j += 1;
                    }
                    if j < b.len() && (0x40..=0x7e).contains(&b[j]) {
                        i = j + 1; // consumed CSI incl. final byte
                        continue;
                    }
                    // No final byte → falls through to the lone-ESC rule.
                }
                // Try OSC: ESC ] [^BEL,ESC]* (BEL | ESC \)
                if b.get(i + 1) == Some(&b']') {
                    let mut j = i + 2;
                    while j < b.len() && b[j] != 0x07 && b[j] != 0x1b {
                        j += 1;
                    }
                    if j < b.len() && b[j] == 0x07 {
                        i = j + 1; // BEL-terminated
                        continue;
                    }
                    if j + 1 < b.len() && b[j] == 0x1b && b[j + 1] == b'\\' {
                        i = j + 2; // ST-terminated
                        continue;
                    }
                    // Unterminated OSC → the regex doesn't match; fall to lone-ESC.
                }
                // lone ESC: \x1b[@-_]?  (ESC + optionally ONE byte in 0x40..=0x5f)
                if b.get(i + 1).is_some_and(|&n| (0x40..=0x5f).contains(&n)) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn w6_strip_ansi_agrees_on_csi_and_osc_classes() {
        // The CSI + OSC classes the TS regex targets (utils.ts:373-383) — the ONLY
        // escapes a claude composer dump carries. boot::strip_ansi and the TS
        // reference must produce identical output on each; a regression in either
        // stripper trips this.
        let cases: &[&str] = &[
            // OSC title set, BEL-terminated.
            "\u{1b}]0;window title\u{07}keep",
            // OSC hyperlink, ST-terminated (ESC \).
            "before\u{1b}]8;;https://x\u{1b}\\link\u{1b}]8;;\u{1b}\\after",
            // CSI colour run + cursor moves around text.
            "\u{1b}[31mred\u{1b}[0m \u{1b}[2J\u{1b}[1;1Hhome",
            // CSI with ? private param + intermediate bytes.
            "\u{1b}[?25ltext\u{1b}[0 q",
            // Lone ESC followed by a [@-_] byte (BOTH consume ESC + the one byte).
            "a\u{1b}Mb",
            // Lone trailing ESC at end-of-input (BOTH drop it).
            "trail\u{1b}",
            // Plain text, no escapes (identity).
            "just plain text 123",
            // Mixed CSI + OSC + plain.
            "\u{1b}[1mbold\u{1b}[0m\u{1b}]0;t\u{07}done",
            // Unicode payload between escapes (composer Unicode survives both).
            "\u{1b}[2mcafé ☕ 日本語\u{1b}[0m",
        ];
        for c in cases {
            assert_eq!(
                strip_ansi(c),
                ts_strip_ansi_reference(c),
                "boot::strip_ansi diverges from TS stripAnsi on CSI/OSC input {c:?}"
            );
        }
    }

    #[test]
    fn w6_documented_lone_esc_divergence() {
        // The DOCUMENTED, FLAGGED divergence (see the section comment above): on a
        // BARE lone ESC followed by a byte OUTSIDE 0x40..=0x5f, boot consumes the
        // byte (its wide catch-all rule, sanctioned by boot.rs's keypad test)
        // whereas TS's `\x1b[@-_]?` keeps it. This is inert for the composer
        // (composer dumps carry no such bare runs — those bytes only appear inside
        // CSI/OSC payloads, which both strip identically). Pinned so any change to
        // EITHER stripper surfaces here for the lead's review rather than silently.
        // ESC = (keypad, 0x3d): boot strips the `=`, TS keeps it.
        assert_eq!(strip_ansi("plain\u{1b}=more"), "plainmore");
        assert_eq!(ts_strip_ansi_reference("plain\u{1b}=more"), "plain=more");
        // ESC x (0x78): boot strips the `x`, TS keeps it.
        assert_eq!(strip_ansi("\u{1b}xtext"), "text");
        assert_eq!(ts_strip_ansi_reference("\u{1b}xtext"), "xtext");
    }
}
