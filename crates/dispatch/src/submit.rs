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
use std::path::PathBuf;

// The PURE submit discipline now lives in the `quorum-submit-discipline` LEAF
// crate (mirrors the `quorum-delivery-events` precedent re-exported by
// `crate::events`, events.rs:69). Re-exported here so every existing call site —
// `crate::submit::*`, `dispatch::submit::*` from the bin/tests, and the concrete
// `Real*` bindings below (which impl these traits on dispatch-local types) — keeps
// resolving byte-for-byte unchanged. The W8 transcript-verify / `--wait` layer and
// the `Real*` (Mux/Clock/fs) bindings stay in this module.
pub use quorum_submit_discipline::{
    chunk_text, deliver_idle_two_write, deliver_idle_two_write_with, deliver_prompt,
    send_text_chunked, verify_accepted_then_cr, ChunkSendOptions, ContentVerifiedSubmit,
    DeliverDeps, DeliverOutcome, IdleDeliverDeps, SubmitDeps, SubmitOptions, SubmitOutcome,
    CHUNK_BYTES, CHUNK_SETTLE_MS, DELIVER_TIMEOUT_S, TWO_WRITE_SETTLE_MS,
};

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
/// warn and the priming chain stalled (no `message-seen` ⇒ the consumer's on-received gate
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
/// RE-SCAN fix (PTY-FIX-DESIGN.md §4). When a consumer fires a chain link at a child that
/// is still mid-turn, `send:pty` classifies it `SendQueue` and (pre-fix) returned
/// without any verify → the busy-queued path was STRUCTURALLY incapable of emitting
/// `message-seen` → a create-head `--via pty` chain of ≥3 links stalled at link-2
/// (the consumer's on-received never opened link-3). This extends the proven deferred verify
/// to that path: it polls the (already-resolvable) transcript at/after the CAPTURED
/// PRE-SEND OFFSET — the high-water floor that sits PAST the prior in-flight turn's
/// user record (constraint 1) — until the queued turn's record genuinely lands.
///
/// SEMANTIC MUST (PTY-FIX-DESIGN.md §3): on a unique match the CALLER emits the SAME
/// `message-seen` the idle path emits (→ the consumer's `Fired{Seen}`, trips on-received) —
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
