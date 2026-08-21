//! The `mux_pty` carrier — type a message into a session's mux pane.
//!
//! The FIFTH carrier, and the last one to move. It serves THREE lanes: a
//! relay-less claude pane, a codex `--interactive` pane, and a pi `--interactive`
//! pane are all typed at through this one function ([`super::render`]'s callers
//! know which; this body does not care).
//!
//! Nothing here prints and nothing here exits; see [`super`].
//!
//! ── WHERE THIS ONE IS CUT, AND WHY NOT AT THE FUNCTION BOUNDARY ─────────────
//! `send::run_send_pty_resolved` was ~1100 lines with 41 `eprintln!` and 6
//! `println!` across 25 return sites, and its printing is genuinely interleaved
//! with its control flow — `eprint!("Waiting for response")` with no newline, a
//! progress glyph written per 500ms poll, then `eprintln!(" done")` closing the
//! same line. Those cannot become [`Notes`]: a note is a whole line, and a line
//! that is written in three pieces across a 120-second loop is not one.
//!
//! So the cut is at the **`wait`/`raw`/`full` boundary**, not at the function
//! boundary. The `qd send:pty` carrier is called with
//! `wait=false, raw=false, full=false`, so the DELIVERY half — provider refusal,
//! pane/attendance refusal, transcript snapshot, `send-initiated`, the embedded
//! handoff, the zmx busy-queued and idle write paths, the W8 verify and its two
//! deferred variants — is reachable from it and lives here. `raw`/`full` are not
//! parameters of `send_mux_pty` at all: they only pick an [`ExtractMode`] for the
//! capture on the other side of that boundary.
//!
//! ── AND WHERE THE OTHER SIDE OF IT WENT ─────────────────────────────────────
//! Phase 3B left the `--wait` reply capture in `bin/qd/verbs/send.rs`, and with it
//! three emissions: `turn-anchored` on its Complete arm, `status-transition` per
//! observed status change, and the `WatchGuard`'s Drop terminal. Those are
//! qw-owned records written from a qd verb, so the loop's WRITING half followed
//! the carrier here — [`run_anchor_wait`] and its write-free twin
//! [`run_reply_capture`], reached through [`PtyOutcome::Await`] as before.
//!
//! The LINE argument above still stands and is why the split is where it is: the
//! banner, the five closing words and the stdout body remain the verb's, because
//! none of them can be a [`Notes`] entry. [`ReplyOutcome`] is the answer they
//! render. The one thing that still writes to a terminal from in here is the
//! per-poll progress GLYPH, which is a character rather than a line and has no
//! meaning if it is returned instead of written — stderr, inherited across the
//! process cut by D5, exactly as [`super::render`]'s lines are.
//!
//! ── STRICT ─────────────────────────────────────────────────────────────────
//! `strict=true` maps known-failed submission (busy-lane "still in the composer",
//! idle-lane "never went busy") to a non-zero exit instead of warn-and-continue.
//! Every carrier caller sets it; the explicit `qd send:pty` verb passes `false`
//! and keeps its historic warning+exit-0 contract.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use crate::boot::{read_pid_status, Sleeper};
use crate::delivery::{CarrierError, Delivered, Notes, Refused};
use crate::effects::{Clock, Env};
use crate::events::{self, Anchor, EventWriter, Payload, WatchGuard};
use crate::model::{Session, SessionStatus};
use crate::mux::Mux;
use crate::paths::QdPaths;
use crate::sendpty::{
    capture_or_defect, composer_holds_message, decide_send_pty, parse_jsonl_slice, run_wait_loop,
    ExtractMode, ParsedLine, SendPtyAction, WaitDeps, WaitOutcome,
};
use crate::submit::{
    deliver_idle_two_write, send_text_chunked, ChunkSendOptions, IdleDeliverDeps, SubmitOptions,
};
use crate::zmx_dir::resolve_zmx_dir;

// ===========================================================================
// The injected effects, and the owned inputs
// ===========================================================================

/// Everything the pane carrier cannot own.
///
/// Deliberately SMALLER than [`super::SendDeps`]: the pane carrier resolves its
/// own `QdPaths` and its own `Mux`, because both of those resolutions have
/// USER-FACING FAILURE MODES (`HOME` unset; a bogus `QD_MUX`) that are two of its
/// refusals — see [`MuxPtyError::HomeUnset`] and [`MuxPtyError::MuxUnavailable`].
/// Handing them in pre-resolved would have moved those two lines and their exit
/// codes out to the callers, where the two callers would have had to agree by
/// hand on wording the pre-move body owned in one place.
pub struct PtyDeps<'a> {
    pub env: &'a dyn Env,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
}

/// The owned inputs of one pane delivery.
///
/// `wait` is here and `raw`/`full`/`timeout` are not, and that asymmetry IS the
/// cut: `wait` changes what this body DOES (a hard transcript precondition, an
/// anchored rather than seen terminal, and an early hand-back instead of a receipt
/// line), while the other three belong entirely to the reply capture that runs
/// after this body has already answered — `raw`/`full` pick its `ExtractMode` and
/// `--timeout` bounds its loop. This body reads none of the three, which is why
/// none of them is a field: a parameter a core ignores is a claim about the core
/// that is not true.
pub struct PtyParams<'a> {
    pub session: &'a Session,
    pub message: &'a str,
    /// The send id the CALLER minted, before this body ran.
    ///
    /// This used to be `events::mint_send_id(clock)` a few lines into the body,
    /// which meant nobody outside could name the send until the body had already
    /// written to the pane. See [`crate::contract::Message::id`] for why that had
    /// to change: qd's intent record has to be durable BEFORE the delivery, and
    /// it can only be written against an id that already exists.
    pub send_id: &'a str,
    /// See [`PtyOutcome::Await`]. Carriers always pass `false`.
    pub wait: bool,
    /// See the module docs. Carriers always pass `true`.
    pub strict: bool,
}

// ===========================================================================
// What this carrier answers with
// ===========================================================================

/// The delivery half's answer.
///
/// `Ok(Done)` is the whole story and renders through [`super::render`] like any
/// other carrier. `Ok(Await)` can only happen with `wait=true`, i.e. never from a
/// carrier — it is the hand-back to `qd send:pty`'s reply-capture half.
pub enum PtyOutcome {
    /// The send is finished. `wait=false` always ends here.
    Done(Delivered),
    /// `wait=true`, and delivery succeeded: the caller runs its own `--wait`
    /// phase from [`PtyAwait`].
    Await(PtyAwait),
}

/// Which `--wait` phase the caller must run, and everything it needs to run it.
///
/// Every field is something this body RESOLVED or MINTED. Nothing here is a
/// convenience copy of an input the caller already holds: the caller keeps its own
/// `Session`, `--timeout`, `--raw` and `--full`.
pub struct PtyAwait {
    /// Notes accumulated BEFORE the hand-back. The caller prints them
    /// ([`super::render_notes`]) before it writes its first mid-line byte.
    pub notes: Notes,
    /// Which of the two `--wait` phases applies.
    pub phase: WaitPhase,
    /// The `send-initiated` id every terminal for this send joins on.
    pub send_id: String,
    /// The display label every `--wait` line names the session by.
    pub label: String,
    /// The resolved transcript. `None` only on [`WaitPhase::EmbeddedTerminal`],
    /// whose confirmed-delivery arm degrades to "reply not captured" rather than
    /// failing; [`WaitPhase::AnchorLoop`] cannot reach the hand-back without one,
    /// because [`MuxPtyError::NoTranscript`] refused first.
    pub jsonl_path: Option<PathBuf>,
    /// The transcript length snapshotted BEFORE the first byte was written.
    pub start_offset: u64,
    /// The W8 idle verify already emitted `turn-anchored` for this send, so the
    /// anchor loop's Complete arm must NOT emit a second one (m-1: one landed
    /// signal per `send_id`).
    pub verify_anchored: bool,
    /// The `.claude`-layout paths (`sessions_dir` for the pid file,
    /// `projects_dir`).
    pub paths: QdPaths,
    /// The QD_HOME-honouring paths the delivery log lives under. The SAME
    /// derivation this body wrote its events through, handed over rather than
    /// re-derived, so a `--wait` that reads the ledger cannot end up reading a
    /// different file from the one that was written.
    pub ev_paths: QdPaths,
}

/// The two `--wait` phases, which have nothing in common but their flag.
pub enum WaitPhase {
    /// EMBEDDED backend: the mux owns the send's lifecycle, so the caller awaits
    /// a real terminal through `LaneOps::await_terminal`.
    EmbeddedTerminal,
    /// ZMX backend: the caller runs the JSONL anchor loop for the reply.
    AnchorLoop,
}

// ===========================================================================
// Why this carrier could not deliver
// ===========================================================================

/// ADD-18 (ack3-spec §5.1) exit code for a `send:pty` PTY-write failure.
///
/// EXIT-CODE TRIPLE for the send/boot verbs (machine-readable surface, §5.1):
///   1  = generic / infra / arg errors (the broad class)
///   10 = `new -p` Stalled — went-busy acceptance never landed (ADR-0008;
///        lifecycle.rs Stalled path)
///   11 = `send:pty` PTY write failed — one or more chunks' `mux.send` returned
///        Err (immediate daemon Error OR the bounded OP_TIMEOUT); ADD-18.
/// `send:pty` SUCCESS = 0, `--wait` timeout = 1 (unchanged).
pub const EXIT_PTY_WRITE_FAILED: i32 = 11;

/// Why the pane carrier could not deliver.
///
/// DELIBERATELY NO `Display` — see [`CarrierError`]. Exactly ONE variant is
/// verb-attributed ([`MuxPtyError::UnknownProvider`]); the rest are lines the
/// pre-move body wrote bare, and they ignore the verb, which is the honest
/// encoding of "this line was never verb-attributed".
#[derive(Debug)]
pub enum MuxPtyError {
    /// A resolved row whose provider is neither `claude-code` nor `opencode`.
    /// The shared acting-verb refusal (`common::refuse_unknown_provider`'s
    /// wording, carried verbatim). STRUCTURALLY UNREACHABLE today — the join
    /// defaults an absent provider to claude-code — and armed rather than
    /// dropped.
    UnknownProvider { provider: String },
    /// The row is `Cold`.
    Cold,
    /// The row has no live mux session linked. Two wordings, keyed off the
    /// SELECTED BACKEND rather than off zmx: under the embedded backend the
    /// legacy "not in zmx" text named the wrong layer (C1 redfix), and the zmx
    /// arm keeps the byte-stable legacy wording.
    NoMuxPane { embedded: bool },
    /// M3 defensive refusal of an ATTENDED zmx target. Carries the message
    /// `crate::sendpty::attended_zmx_refusal_message` already rendered, because
    /// the unknown-count arm's wording is that function's decision, not this
    /// enum's.
    AttendedZmx { text: String },
    /// `HOME` is unset, so neither the session state dir nor the mux socket dir
    /// resolves.
    HomeUnset,
    /// `--wait`'s HARD precondition: without a transcript there is no anchor to
    /// wait on. Unreachable from any carrier (`wait=false`).
    NoTranscript,
    /// A bogus `QD_MUX`, or a backend that could not be built. Carries the
    /// selector's own message and its DISTINCT exit code (G-SEL/G-NEG) — this
    /// refusal is the only one that does not exit 1.
    MuxUnavailable { message: String, code: i32 },
    /// The embedded mux refused the handoff BEFORE accepting it: nothing spooled,
    /// no dangling send, and (deliberately) no qd-minted terminal.
    HandoffFailed { label: String, detail: String },
    /// ADD-18 (§5.1): one or more text chunks' `mux.send` returned Err. §C2: an
    /// IN-BAND-UNDETERMINABLE outcome, so the door mints NO terminal — the send
    /// stays dead-dangling and `qd delivery:recover` closes it.
    WriteFailed { acked: u32, total: u32 },
    /// QS-3, busy lane, `strict` only: the composer provably still holds our text
    /// after the remediation CR. SILENT — the warning is already a note, and the
    /// pre-move body printed nothing more before its `return 1`.
    StuckComposer,
    /// QS-3, idle lane, `strict` only: the session never went busy. SILENT for
    /// the same reason as [`MuxPtyError::StuckComposer`], and deferred to AFTER
    /// the `chunks-delivered` emit so the event stream is complete before the
    /// exit (symmetry with the busy lane).
    NotAccepted,
    /// W8, >1-chunk: the read-back found our text truncated in the transcript.
    /// `turn-anchored-mismatch` is emitted before this refusal.
    PayloadTruncated {
        label: String,
        expected: usize,
        recorded: usize,
    },
    /// W8, >1-chunk: no user record appeared within the verify window.
    PayloadNoRecord { label: String, timeout_s: u64 },
}

impl CarrierError for MuxPtyError {
    fn line(&self, verb: &str) -> Option<String> {
        Some(match self {
            MuxPtyError::UnknownProvider { provider } => format!(
                "qd {verb}: unknown provider \"{provider}\" — this engine supports: claude-code."
            ),
            MuxPtyError::Cold => "Session is dead. Use 'qd resume' first.".to_string(),
            MuxPtyError::NoMuxPane { embedded: true } => {
                "Session has no live qrmux session — cannot send (it may still be \
                     starting up; retry in a moment)."
                    .to_string()
            }
            MuxPtyError::NoMuxPane { embedded: false } => {
                "Session is not in zmx — cannot send.".to_string()
            }
            MuxPtyError::AttendedZmx { text } => text.clone(),
            MuxPtyError::HomeUnset => {
                "qd: HOME is not set — cannot resolve the session state dir.".to_string()
            }
            MuxPtyError::NoTranscript => "Cannot find conversation JSONL file.".to_string(),
            MuxPtyError::MuxUnavailable { message, .. } => message.clone(),
            MuxPtyError::HandoffFailed { label, detail } => {
                format!("Could not hand \"{label}\" its send to the mux delivery surface: {detail}")
            }
            MuxPtyError::WriteFailed { acked, total } => {
                format!("ERROR: PTY write failed ({acked}/{total} chunks acked) — see events file")
            }
            // The two strict-mode exits whose account was already given as a note.
            MuxPtyError::StuckComposer | MuxPtyError::NotAccepted => return None,
            MuxPtyError::PayloadTruncated {
                label,
                expected,
                recorded,
            } => format!(
                "ERROR: payload truncated in delivery to \"{label}\": expected \
                 {expected} bytes, recorded {recorded}.\n  The message submitted — \
                 do NOT blindly resend (double-submit risk).\n  Attach: qd attach {label}"
            ),
            MuxPtyError::PayloadNoRecord { label, timeout_s } => format!(
                "ERROR: could not verify payload arrival in \"{label}\"'s \
                 transcript within {timeout_s}s (no user record appeared).\n  \
                 Attach: qd attach {label}"
            ),
        })
    }

    fn exit_code(&self) -> i32 {
        match self {
            MuxPtyError::WriteFailed { .. } => EXIT_PTY_WRITE_FAILED,
            MuxPtyError::MuxUnavailable { code, .. } => *code,
            _ => 1,
        }
    }
}

// ===========================================================================
// The pane-write recorders (ACK-2 §9)
// ===========================================================================

/// Per-chunk ack record (ACK-2 §9 recorder, red-team R9): how many text chunks
/// were written and how many `mux.send` calls returned `Ok`. ALL acked
/// (`acked == total`) ⇒ the send's text fully delivered → emit `chunks-delivered`.
///
/// ADD-18 (ack3-spec §5): a partial/total write failure (`!all_ok()`) is the
/// `send:pty` WRITE-FAILED class — it exits 11 (no longer F1's "discarded
/// result"). `chunks-delivered` stays keyed on `all_ok()`, so a failed write leaves
/// the send-initiated record present and chunks-delivered ABSENT (that file
/// signature is the recovery-read evidence; §5.1). Partial-ack IS failure (message
/// integrity is all-or-nothing).
#[derive(Clone, Copy)]
pub struct ChunkAcks {
    pub total: u32,
    pub acked: u32,
}

impl ChunkAcks {
    pub fn all_ok(&self) -> bool {
        self.total > 0 && self.acked == self.total
    }

    /// The [`MuxPtyError::WriteFailed`] this tally refuses with.
    fn write_failed(&self) -> MuxPtyError {
        MuxPtyError::WriteFailed {
            acked: self.acked,
            total: self.total,
        }
    }
}

/// Send `text` to a session's PTY as ≤1024B code-point-safe chunks with a ~150ms
/// inter-chunk settle (ADR 0009 mode (a): a single large `zmx send` overflows the
/// tty queue and drops wholesale). For text ≤1024B this is a single byte-identical
/// `mux.send`. The SHARED chunking helper drives the per-chunk `mux.send` + sleep.
///
/// ACK-2 §9: RECORDS each chunk's `mux.send` ack (the closure previously discarded
/// it via `let _ =`). ADD-18 (ack3-spec §5) RETIRES the old F1 "discarded result"
/// posture: the recorded tally now DRIVES the exit code — `!all_ok()` after the
/// send stage exits 11. The tally still gates `chunks-delivered` (only a full ack
/// emits it).
pub fn send_text_chunked_mux(
    mux: &dyn Mux,
    sleeper: &dyn Sleeper,
    dir: &Path,
    zmx_name: &str,
    text: &str,
) -> ChunkAcks {
    let mut total: u32 = 0;
    let mut acked: u32 = 0;
    send_text_chunked(
        &mut |chunk| {
            total += 1;
            if mux.send(dir, zmx_name, chunk).is_ok() {
                acked += 1;
            }
        },
        &mut |ms| sleeper.sleep_ms(ms),
        text,
        ChunkSendOptions::default(),
    );
    ChunkAcks { total, acked }
}

/// The `chunks-delivered.ack_source` channel name (§2.3.2): the embedded backend
/// blocks on the daemon's per-write `InputSent` ack ("input-sent"); the zmx
/// backend observes only the `zmx send` process exit 0 ("cli-exit", weaker —
/// named honestly). STILL NOT A RECEIPT (rev C §2.1).
pub fn ack_source_label(env: &dyn Env) -> &'static str {
    if is_zmx_backend(env) {
        "cli-exit"
    } else {
        "input-sent"
    }
}

/// Is the ZMX backend selected, for backend-keyed wording and event labels?
///
/// The qw half of the binary's `common::send_backend_label`, with its fallback
/// preserved exactly: a bogus `QD_MUX` (already rejected upstream on every path
/// that reaches here) answers EMBEDDED — the C1 default — so the worst case names
/// the default backend rather than the stale zmx wording.
fn is_zmx_backend(env: &dyn Env) -> bool {
    matches!(
        crate::mux_selector::parse_backend(env),
        Ok(crate::mux_selector::Backend::Zmx)
    )
}

/// An [`IdleDeliverDeps`] that RECORDS each `send_text` chunk's `mux.send` ack
/// (ACK-2 §9 recorder for the idle two-write path; red-team R9). It mirrors
/// [`crate::submit::RealIdleDeliverDeps`] field-for-field EXCEPT `send_text`, which
/// captures `mux.send(...).is_ok()` instead of discarding it (the Real impl does
/// `let _ = mux.send`). The `IdleDeliverDeps` trait and the pure cores are
/// UNTOUCHED. The CR write (`send_cr`) is NOT counted (only text chunks are
/// chunks-delivered evidence).
struct RecordingIdleDeliverDeps<'a> {
    mux: &'a dyn Mux,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
    zmx_name: String,
    pid_file: PathBuf,
    dir: PathBuf,
    /// Per-chunk ack tally (`Cell` — `IdleDeliverDeps::send_text` takes `&self`).
    total: Cell<u32>,
    acked: Cell<u32>,
}

impl RecordingIdleDeliverDeps<'_> {
    fn acks(&self) -> ChunkAcks {
        ChunkAcks {
            total: self.total.get(),
            acked: self.acked.get(),
        }
    }
}

impl IdleDeliverDeps for RecordingIdleDeliverDeps<'_> {
    fn send_text(&self, text: &str) {
        self.total.set(self.total.get() + 1);
        if self.mux.send(&self.dir, &self.zmx_name, text).is_ok() {
            self.acked.set(self.acked.get() + 1);
        }
    }
    fn send_cr(&self) {
        let _ = self.mux.send(&self.dir, &self.zmx_name, "\r");
    }
    fn read_screen(&self) -> String {
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

/// `zmx history <name>` pinned to `op_dir` — the composer screen for the content-
/// verified CR (send.ts readScreen). Errors yield an empty screen (a missing
/// screen can't hold our message → not stuck → no CR; fail-safe).
fn read_screen(mux: &dyn Mux, op_dir: &Path, zmx_name: &str) -> String {
    mux.history(op_dir, zmx_name).unwrap_or_default()
}

/// `parseInt(opts.timeout) * 1000` (send.ts:213). A non-integer default never
/// occurs (clap default "120"); a garbage explicit value → 0 (JS NaN*1000 → NaN,
/// but the loop's `< timeoutMs` is then never true → immediate timeout; we clamp
/// to 0 which has the same effect).
pub fn timeout_ms(raw: &str) -> i64 {
    raw.trim().parse::<i64>().unwrap_or(0).saturating_mul(1000)
}

/// A6 §4.1: append a content-free invoked line for a SUCCESSFUL send (any exit-0
/// path). Best-effort — a failure produces a WARNING note and NEVER changes the
/// verb's exit code (spec §4.1: telemetry must never break a working send).
///
/// Distinct from [`super::append_send_invoked`], which the relay fast path uses:
/// this path always has a resolved session id, so the line is keyed on it as well
/// as on the name.
pub fn append_pty_invoked(
    env: &dyn Env,
    clock: &dyn Clock,
    session_id: &str,
    name: Option<&str>,
) -> Option<String> {
    match crate::telemetry::append_invoked(env, clock, "send", Some(session_id), name) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "WARNING: telemetry invoked append failed (non-fatal): {e}"
        )),
    }
}

/// The shared acting-verb provider gate (`common::refuse_unknown_provider`'s
/// classification half). `Some(provider)` ⇒ refuse.
fn unknown_provider(s: &Session) -> Option<String> {
    match s.provider.as_str() {
        "claude-code" | "opencode" => None,
        other => Some(other.to_string()),
    }
}

/// Build the backend-selected mux, keeping the binary's THREE distinct failures
/// distinct: `HOME` unset, an unparseable `QD_MUX`, and a backend that could not
/// be built. `crate::lanes::build_real_mux` collapses all three to one `String`,
/// which loses the selector's exit code — and the selector's exit code is part of
/// this verb's machine-readable surface (G-SEL/G-NEG).
fn resolve_mux(env: &dyn Env) -> Result<Box<dyn Mux>, MuxPtyError> {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return Err(MuxPtyError::HomeUnset);
    };
    let home = PathBuf::from(home);
    let backend =
        crate::mux_selector::parse_backend(env).map_err(|e| MuxPtyError::MuxUnavailable {
            message: e.message,
            code: e.exit_code,
        })?;
    crate::mux_selector::select_mux(backend, &home, env).map_err(|e| MuxPtyError::MuxUnavailable {
        message: e.message,
        code: e.exit_code,
    })
}

/// A refusal that fires AFTER the id was minted, carrying both the notes written
/// so far and the id, so a later `qd delivery:recover` still has something to
/// search on. See [`Refused::message_id`].
fn refuse(notes: &mut Notes, send_id: &str, error: MuxPtyError) -> Refused<MuxPtyError> {
    Refused {
        notes: std::mem::take(notes),
        error,
        message_id: Some(send_id.to_string()),
    }
}

// ===========================================================================
// The carrier
// ===========================================================================

/// The lane's entry: deliver `message` into `session`'s pane and render the
/// answer.
///
/// `wait=false, strict=true` — the two values `send::run_send_pty_unified`
/// hard-coded before the body moved. Its third, `timeout="120"`, has no field to
/// go in: `--timeout` bounds the reply capture, which `wait=false` never reaches.
///
/// `"send:pty"` is the verb stamped on [`MuxPtyError::UnknownProvider`], and it is
/// the verb the pre-move body hard-coded — for `qd send` as much as for
/// `qd send:pty`, so a `qd send` that hits that refusal names a verb the user did
/// not type. Preserved rather than corrected here, exactly as the four daemon
/// carriers' `"send:relay"` is. It is a REPORTED finding, not an accepted one —
/// and structurally unreachable today besides.
pub fn deliver_mux_pty(
    deps: &PtyDeps<'_>,
    session: &Session,
    message: &str,
    send_id: &str,
) -> crate::lanes::CarrierOutcome {
    let params = PtyParams {
        session,
        message,
        send_id,
        wait: false,
        strict: true,
    };
    match send_mux_pty(deps, &params) {
        // `wait=false` cannot reach the hand-back, and the alternative to saying
        // so is a `CarrierOutcome` invented for a state that does not occur.
        Ok(PtyOutcome::Await(_)) => unreachable!("wait=false never yields PtyOutcome::Await"),
        Ok(PtyOutcome::Done(d)) => super::render::<MuxPtyError>(Ok(d), "send:pty"),
        Err(r) => super::render(Err(r), "send:pty"),
    }
}

/// Type `message` into the session's mux pane.
///
/// Port of the `send:pty` action (qa/hardening@3dd9f1e:src/commands/send.ts:100-336),
/// carried here from `send::run_send_pty_resolved` with its control flow intact:
/// every one of its early returns is still an early return, in the same place, and
/// every line it wrote is still written — as a [`Notes`] entry when it was
/// mid-flow, as a [`MuxPtyError`] when it ended the send. See the module docs for
/// what stayed behind and why.
pub fn send_mux_pty(
    deps: &PtyDeps<'_>,
    params: &PtyParams<'_>,
) -> Result<PtyOutcome, Refused<MuxPtyError>> {
    let env = deps.env;
    let clock = deps.clock;
    let sleeper = deps.sleeper;
    let session = params.session;
    let message = params.message;
    let wait = params.wait;
    let strict = params.strict;

    // The notes this body accumulates, in emission order.
    let mut notes = Notes::new();

    // codex P1, R1 (codex-p1-spec section 2.3): refuse an unknown provider LOUDLY.
    //
    // codex-interactive: scoped to rows with no resolvable hosting (the
    // `attach_resolved` / `kill` narrowing). This body is provider-generic — it
    // writes to a pane through the mux — and a pane-hosted codex row is routed
    // here ON PURPOSE by the unified selector. The allow-list this helper carries
    // predates codex having a pane, so an unconditional call would refuse the very
    // session the selector just decided this path serves.
    if crate::provider::row_hosting(&session.provider, session.hosting.as_deref()).is_none() {
        if let Some(provider) = unknown_provider(session) {
            return Err(MuxPtyError::UnknownProvider { provider }.into());
        }
    }

    // cold → dead (send.ts:105-108); no zmxName → not-in-zmx (send.ts:110-113).
    if session.status == SessionStatus::Cold {
        return Err(MuxPtyError::Cold.into());
    }
    let zmx_backend = is_zmx_backend(env);
    let Some(zmx_name) = session.zmx_name.clone() else {
        // WRONG-LAYER FIX (C1 redfix): this condition is "the session has no live
        // mux session linked" — generic, NOT zmx-specific. Under the embedded
        // backend the stale "not in zmx" wording named the wrong layer. Key the
        // text off the selected backend: zmx keeps the BYTE-STABLE legacy wording;
        // embedded names the actual backend (qrmux). A bogus QD_MUX here would have
        // already failed `all_sessions` above, so the parse is effectively infallible
        // on this path; on the impossible error we fall back to the generic wording.
        return Err(MuxPtyError::NoMuxPane {
            embedded: !zmx_backend,
        }
        .into());
    };

    // M3 DEFENSIVE REFUSAL of an ATTENDED zmx target (BUILD-DIRECTIVES §1 ruling
    // (2), SUPERSEDING the plan's "zmx keeps legacy" for the attended case). The zmx
    // backend cannot host the polite machinery (journal/lock/countdown), so a blind
    // primary CR into an attended pane could clobber a human's in-progress draft.
    // Attendance is OBSERVED at the protocol seam — `zmx list`'s `clients=N` →
    // `session.zmx_clients` (the same signal reconcile.rs uses for "attached ⇒ never
    // touch") — never guessed. Sound ONLY for zmx: the embedded backend synthesizes
    // `clients = 0` (it observes attendance internally in the mux and defers there),
    // so gating on the zmx backend is load-bearing. The unattended zmx path
    // (clients == 0) keeps today's behavior + honest events untouched below.
    if crate::sendpty::refuse_attended_zmx(zmx_backend, session.zmx_clients) {
        // M5/T2 — fail CLOSED on an unknown count, with an HONEST message that never
        // asserts an attach we did not observe (the unknown arm says the count is
        // unreadable and attendance cannot be ruled out).
        return Err(MuxPtyError::AttendedZmx {
            text: crate::sendpty::attended_zmx_refusal_message(
                session.name.as_deref().unwrap_or(&session.session_id),
                session.zmx_clients,
            ),
        }
        .into());
    }

    let action = decide_send_pty(session.status.as_str());

    // L9a paths from HOME (for the jsonl projects dir + pid file dir).
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return Err(MuxPtyError::HomeUnset.into());
    };
    let paths = QdPaths::from_home(Path::new(&home));

    // --wait preconditions FIRST (parity, send.ts:116-123): resolve the jsonl
    // path + record the size BEFORE sending, so the anchor loop only sees records
    // written by/after our message. W8 (wart-wave-spec §4): a CHUNKED payload
    // resolves + snapshots too — the verify-after-submit read-back needs the
    // pre-delivery offset. --wait keeps its hard requirement (loud exit 1);
    // verify-only resolution failure DEGRADES (warned at the verify step — the
    // send itself must not gain a new precondition).
    let needs_verify = crate::submit::payload_needs_verify(message);
    // ACK-2 D10 (R6/R7): the transcript-offset snapshot is
    // UNCONDITIONAL-WHEN-RESOLVABLE — we ALWAYS try to resolve the jsonl path +
    // stat its length so the `send-initiated` event carries the recovery-read
    // window key, even on a bare (no --wait, single-chunk) send. The verb's OWN
    // preconditions are unchanged: only `--wait` still loudly requires the
    // transcript (exit 1 below); a resolution failure remains non-fatal for the
    // event (the fields are simply absent) and for the verify path (it degrades).
    // codex P1 W7 (codex-p1-spec section 7.2): the transcript-location fallback
    // dispatches through the provider seam, keyed on this row's provider value.
    // send:pty's W1 refusal (the unknown-provider gate, above) already gated an
    // unknown provider, so the value here is claude-code or the parked opencode;
    // `provider_for` resolves claude-code and degrades opencode (parked, not a
    // Provider impl) to the claude derivation for path resolution (same
    // render-survival posture as the gather). The resolved path is byte-identical
    // to the old `find_jsonl_path` call (ClaudeProvider::transcript_path delegates).
    let provider = crate::provider::provider_for(&session.provider)
        .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
    let jsonl_path: Option<PathBuf> = session
        .jsonl_path
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            // pi-interactive: resolve the root through the PROVIDER, not the
            // claude `projects_dir`.
            //
            // This fallback was written when send:pty served claude alone, so the
            // claude root was the only root there was; the provider-routed
            // `transcript_path` then arrived above it and inherited the hard-coded
            // argument. For claude it is still exactly `paths.projects_dir`
            // (`ClaudeProvider::transcript_root` returns it), so nothing about the
            // incumbent path moves — but a codex or pi row reaching here was being
            // asked to find its transcript under claude's tree, where it can never
            // be. The answer was always `None`, which reads as "no transcript yet"
            // and is indistinguishable from the genuine pre-first-turn case.
            //
            // It matters now because the PTY lane's landing verify is what proves
            // a `qd send` was accepted, and a transcript it cannot locate makes
            // every send an honest-but-wrong non-delivery.
            let fx = crate::provider::ProviderFx {
                await_relay: None,
                env,
                paths: &paths,
                socket_dir: PathBuf::new(),
                mux: None,
                clock: None,
                sleeper: None,
                relay: None,
                relay_port: None,
                app_server: None,
                codex_expected_turn_id: None,
                acp_client: None,
                pi_rpc: None,
                acp_pre_dispatch: None,
            };
            provider.transcript_path(
                &provider.transcript_root(&fx),
                &crate::provider::SessionKey {
                    id: &session.session_id,
                    name: session.name.as_deref(),
                    cwd: session.cwd.as_deref(),
                    pid: session.pid,
                },
            )
        });
    let mut start_offset: u64 = 0;
    if let Some(jp) = &jsonl_path {
        start_offset = std::fs::metadata(jp).map(|m| m.len()).unwrap_or(0);
    } else if wait {
        // --wait's hard precondition is UNCHANGED (loud exit 1 when unresolvable).
        return Err(MuxPtyError::NoTranscript.into());
    }

    // Op dir = session.socketDir ?? canonical (Bug D — every write AND every
    // screen read uses this SAME dir so they hit the SAME zmx session,
    // send.ts:130-133).
    let canonical = resolve_zmx_dir(env);
    let op_dir: PathBuf = session
        .socket_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(canonical);

    // Backend-selected mux (C1 D3). A live session carries its socket_dir (tagged
    // by the backend's list), so the embedded lane writes to the qrmux dir.
    let mux_box = resolve_mux(env)?;
    let mux: &dyn Mux = mux_box.as_ref();

    let label = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    // A6 §4.1: capture identity for the content-free invoked line emitted on a
    // SUCCESSFUL send (any exit-0 path below). Best-effort — see
    // [`append_pty_invoked`].
    let invoked_sid = session.session_id.clone();
    let invoked_name = session.name.clone();

    // --- ACK-2 §9: engine event emission (additive, best-effort non-fatal) ----
    // The sessionId is ALWAYS resolved on the send:pty path, so the events file
    // is keyed on it (§4.1; no byname fallback here). The state_dir honors QD_HOME
    // (§4.1 / ADD-14): resolve it via the QD_HOME-aware paths.
    //
    // Bound as the WHOLE `QdPaths`, not just its `state_dir`, because the `--wait`
    // phase this body hands back to needs the same resolution: `QdPaths::from_home`
    // (the `paths` above) builds `state_dir` from HOME alone, so handing THAT to
    // `lane_ops` would point the lane's ledger read at
    // `<home>/.quorum/dispatch/state` while this body's own writer wrote under
    // QD_HOME — the terminal would be written to one file and awaited on another.
    // One derivation, one path.
    let ev_paths = QdPaths::from_home_env(&paths.home, env);
    let ev_state = ev_paths.state_dir.clone();
    let writer = EventWriter::for_key(
        &ev_state,
        &session.session_id,
        Some(session.session_id.clone()),
        session.name.clone(),
    );
    // MINTED BY THE CALLER (`PtyParams::send_id`), not here: qd writes its intent
    // record against this id before the delivery starts, so the id has to exist
    // before this body does anything. The shape is unchanged — it is still
    // `events::mint_send_id`'s `"{pid}-{epoch_ms}-{n}"` — but the pid in it is
    // now the MINTING process's, which since the wire split is qd's rather than
    // the `qw` subprocess's. Nothing parses a send_id (§2.1: opaque, equality
    // only), and the dead-writer rule reads the ENVELOPE pid, never this one.
    let send_id = params.send_id.to_string();

    // §2.3.1 send-initiated: minted BEFORE any chunk write. `send_path` keys off
    // the decided action (busy → queued; idle → idle). The per-chunk shas come
    // from the PRODUCTION splitter (chunk_text(msg,1024)); the count is the same
    // splitter's length. transcript/offset present when resolvable (D10).
    let chunks_vec = crate::submit::chunk_text(message, crate::events::CHUNK_BYTES);
    let chunk_sha256s: Vec<String> = chunks_vec
        .iter()
        .map(|c| events::sha256_hex(c.as_bytes()))
        .collect();
    let send_path = match action {
        SendPtyAction::SendQueue => "busy-queued",
        SendPtyAction::SendVerify => "idle",
    };
    let transcript_str = jsonl_path.as_ref().map(|p| p.display().to_string());
    let transcript_offset = jsonl_path.as_ref().map(|_| start_offset);
    events::warn_emit(
        &writer,
        clock,
        &Payload::SendInitiated {
            send_id: send_id.clone(),
            verb: events::verb_str(false).to_string(),
            send_path: send_path.to_string(),
            content_sha256: events::sha256_hex(message.as_bytes()),
            content_len: message.len() as u64,
            chunks: chunks_vec.len() as u32,
            chunk_sha256s,
            chunk_sha256s_capped: false,
            transcript: transcript_str.clone(),
            transcript_offset,
            // ADD-20 (§6.2): redacted ≤256B preview of the sent text.
            content_preview: Some(quorum_core::redact::redact_for_preview(
                message,
                crate::events::PREVIEW_CAP_BYTES,
            )),
        },
    );
    // From here on every refusal is KEYED: a failed delivery still has a message
    // id, which is exactly what a later `recover` needs to search for.

    // ===================== M3: EMBEDDED handoff path =======================
    // The embedded (qrmux) backend HANDS the send to the mux DELIVERY SURFACE
    // (v5 PendingDelivery) instead of orchestrating raw writes. qd emitted ONLY the
    // pre-handoff `send-initiated` (above); from the `DeliveryQueued` receipt onward
    // the MUX owns the send's lifecycle and emits EXACTLY ONE terminal per send_id
    // to the authoritative ledger (the single-writer split — nothing here writes a
    // terminal on this path). The ZMX backend falls through to today's raw-write
    // orchestration (+ honest events); the attended-zmx refusal above already
    // protected it.
    if !zmx_backend {
        let args = crate::embedded_mux::PendingDeliveryArgs {
            send_id: send_id.clone(),
            data: message.as_bytes().to_vec(),
            content_sha256: events::sha256_hex(message.as_bytes()),
            content_len: message.len() as u64,
            transcript: transcript_str.clone(),
            transcript_offset,
            session: Some(session.session_id.clone()),
            name: zmx_name.clone(),
            // Normal polite send; `priority` (which shortens the countdown ceiling)
            // and deliver-now are attach-side controls, not send:pty's to force.
            priority: false,
        };
        match crate::embedded_mux::embedded_pending_delivery(&op_dir, args) {
            Ok(acked) => debug_assert_eq!(acked, send_id, "mux echoes the qd-minted send_id"),
            Err(e) => {
                // DOOR failure BEFORE the mux accepted: nothing spooled, no dangling
                // send. Loud synchronous exit, NO qd-minted terminal (matching today's
                // door discipline — the standing send-initiated + this loud exit is the
                // C1 account; any post-acceptance terminal is the mux's, never qd's).
                return Err(refuse(
                    &mut notes,
                    &send_id,
                    MuxPtyError::HandoffFailed {
                        label,
                        detail: e.to_string(),
                    },
                ));
            }
        }

        if !wait {
            // The mux resolves to exactly one terminal AFTER we exit — drop-immune,
            // no reader-presence dependency (invariant 3). HONEST: accepted for
            // delivery, resolves asynchronously; NEVER claims "landed" (QS-6).
            //
            // F2 CONTRACT (W8 truncation, explicit — not a silent drop): pre-M3, a
            // chunked no-`--wait` send ran qd's SYNCHRONOUS W8 read-back and exited 1
            // on truncation. Under the single-writer async handoff there is NO terminal
            // in hand at exit, so there is deliberately NO synchronous truncation
            // verdict here: the no-`--wait` send is honestly `queued`, and the mux's
            // LandingProbe surfaces a truncated landing as a `turn-anchored-mismatch`
            // terminal on the ledger (P6 honest-events). That mismatch IS observed by
            // `--wait` (mapped to an honest non-zero exit — see the mismatch arm in the
            // verb shell, proven by `m3_embedded_delivery_e2e::…_detects_truncation_as_mismatch`)
            // and by the on-restart reconcile (M5(c)); it is NOT observable in the
            // no-`--wait` sender's exit code (the sender is gone). Re-entry for a
            // no-`--wait` truncation signal: M5(c) reconcile.
            notes.extend(append_pty_invoked(
                env,
                clock,
                &invoked_sid,
                invoked_name.as_deref(),
            ));
            return Ok(PtyOutcome::Done(Delivered {
                message_id: send_id,
                stdout: Some(format!("Message queued to \"{label}\"")),
                notes,
            }));
        }

        // --wait: the caller awaits a REAL 7-set terminal (NEVER returning on the
        // non-terminal DeliveryQueued ack — a queued ack is NOT delivery).
        return Ok(PtyOutcome::Await(PtyAwait {
            notes,
            phase: WaitPhase::EmbeddedTerminal,
            send_id,
            label,
            jsonl_path,
            start_offset,
            verify_anchored: false,
            paths,
            ev_paths,
        }));
    }

    // ===================== ZMX legacy path (unchanged) =====================
    // m-1 (merge-ruling minor, fixed in-window): when the W8 verify already
    // emitted turn-anchored for this send, the --wait Complete arm must NOT emit
    // a SECOND one — one landed signal per send_id (readers take the first
    // terminal, but a systematic duplicate is noise the gate's exact-sequence
    // assert now forbids).
    let mut verify_anchored = false;

    match action {
        SendPtyAction::SendQueue => {
            // BUSY queued send — CHUNKED two-write delivery + content-verified CR
            // (send.ts:141-179). Chunked `send(text)` [ADR 0009 mode (a): a single
            // large write overflows the tty queue], TWO_WRITE_SETTLE_MS, `send("\r")`.
            let acks = send_text_chunked_mux(mux, sleeper, &op_dir, &zmx_name, message);
            sleeper.sleep_ms(crate::submit::TWO_WRITE_SETTLE_MS);
            let _ = mux.send(&op_dir, &zmx_name, "\r");

            // §2.3.2 chunks-delivered: emitted only when EVERY text chunk's
            // mux.send returned Ok (a partial leaves the send dangling by design).
            if acks.all_ok() {
                events::warn_emit(
                    &writer,
                    clock,
                    &Payload::ChunksDelivered {
                        send_id: send_id.clone(),
                        chunks_acked: acks.acked,
                        ack_source: ack_source_label(env).to_string(),
                    },
                );
            } else {
                // ADD-18 (ack3-spec §5.1): the send stage failed (any chunk's
                // mux.send returned Err — immediate daemon Error or OP_TIMEOUT).
                // SHORT-CIRCUIT here: exit 11 loud, no composer-cleared remediation,
                // no --wait watch (all "future events for this send"). send-initiated
                // already emitted; chunks-delivered correctly absent.
                //
                // §C2 (delivery contract, R5 seam ruling 01KX88WKGP): a partial write
                // is an IN-BAND-UNDETERMINABLE outcome, NOT a determinate failure — a
                // chunk's `mux.send` Err is a client-side ack-timeout that can fire
                // strictly AFTER the daemon's non-cancellable write+flush landed the
                // bytes (F3-VERDICT), and the unconditional `\r` submits the turn. So
                // this door must NOT mint a terminal: a `pending-abandoned` here would
                // permanently FALSE-FAIL a send that actually landed, and (once
                // written) forecloses the recovery-read the verb exists to run. We
                // fail loud (exit 11; record-then-fail-loud preserved — the C1 account
                // is the standing send-initiated + the loud synchronous exit + C2's
                // PENDING-closable state) but emit NO terminal: the send stays
                // dead-dangling once the sender exits, and `qd delivery:recover` closes
                // it from the transcript — a disclosed turn-anchored{recovered} if the
                // content landed, else pending-abandoned{recovery-no-candidate}.
                return Err(refuse(&mut notes, &send_id, acks.write_failed()));
            }

            // CONTENT-VERIFIED remediation (never BLIND-CR): only CR while the
            // composer provably still holds OUR exact text (send.ts:162-179).
            let mut stuck = composer_holds_message(&read_screen(mux, &op_dir, &zmx_name), message);
            // §2.3.7 composer-cleared: a POSITIVE holds→not-holds transition is the
            // only emit (never when the composer was never seen holding — that is
            // ambiguous between cleared and never-arrived).
            let stuck_was_observed = stuck;
            if stuck {
                let _ = mux.send(&op_dir, &zmx_name, "\r");
                sleeper.sleep_ms(crate::submit::TWO_WRITE_SETTLE_MS);
                stuck = composer_holds_message(&read_screen(mux, &op_dir, &zmx_name), message);
            }
            if stuck_was_observed && !stuck {
                events::warn_emit(
                    &writer,
                    clock,
                    &Payload::ComposerCleared {
                        send_id: send_id.clone(),
                    },
                );
            }

            if !wait {
                if stuck {
                    // TS WARNING wording verbatim (send.ts:172-176).
                    notes.push(format!(
                        "WARNING: message may be stuck unsubmitted in \"{label}\"'s composer \
                         — check with: qd attach {label}"
                    ));
                    // QS-3 (unified send): known-stuck submission is a non-zero
                    // result for the primary verb (strict mode only; send:pty
                    // preserves its existing warning+continue contract). SILENT:
                    // the warning above is the whole account.
                    if strict {
                        return Err(refuse(&mut notes, &send_id, MuxPtyError::StuckComposer));
                    }
                }

                // D2 §7-B RE-SCAN (PTY-FIX-DESIGN.md §4) — the busy-queued `!wait`
                // path used to return HERE with NO verify, so it was structurally
                // incapable of emitting `message-seen`: a create-head `--via pty`
                // chain of ≥3 links stalled at link-2 (the consumer's on-received never
                // opened link-3). Extend the proven deferred verify to this path:
                // a BOUNDED, high-water-anchored, uniqueness-checked transcript poll.
                // On the queued turn genuinely landing past the captured pre-send
                // floor, emit the SAME `message-seen` the idle path emits (semantic
                // MUST, §3 — NOT `turn-anchored`); on miss/ambiguity degrade to a
                // VISIBLE PENDING (no emit, no false-fire), mirroring the idle
                // NoRecord no-wait discipline. SELF-SCOPED to busy-queued
                // (constraint 4): purely additive on this previously-empty emit
                // path. SEPARATE 120s bound (constraint 2) — a busy-queued turn
                // only lands after the prior in-flight turn completes.
                match &jsonl_path {
                    Some(jp) => {
                        // Constraint 1: floor = the PRE-SEND offset (`start_offset`,
                        // snapshotted BEFORE typing), which sits PAST the prior
                        // in-flight turn's user record → the scan can never anchor on
                        // it (even an identical body), only on THIS turn.
                        let bqdeps = DeferredBusyQueuedDeps {
                            jsonl_path: jp.clone(),
                            pre_send_offset: start_offset,
                            clock,
                            sleeper,
                        };
                        match crate::submit::deferred_verify_busy_queued(
                            &bqdeps,
                            message,
                            crate::submit::BUSY_QUEUED_VERIFY_TIMEOUT_S,
                            crate::submit::VERIFY_POLL_MS,
                        ) {
                            crate::submit::PayloadVerifyOutcome::Verified => {
                                // The queued turn landed uniquely past the floor →
                                // the on-received `message-seen` fires (the consumer's
                                // reason=Seen gate opens the next link). Joined by
                                // send_id (transport-agnostic) — same emit the idle
                                // and fresh-child resolvable paths use.
                                emit_w8_message_seen(&writer, clock, &send_id, message);
                            }
                            _ => {
                                // Miss / ≥2 identical bodies past the floor
                                // (Unattributable) / transcript unresolvable
                                // (SourceUnavailable) → PENDING, never a wrong-fire
                                // (anti-phantom, constraint 3). The promise stays
                                // visibly PENDING — symmetric with the idle NoRecord
                                // no-wait degrade.
                                notes.push(format!(
                                    "WARNING: could not verify the queued payload landed in \
                                     \"{label}\"'s transcript within {}s (the promise stays \
                                     PENDING) — check: qd attach {label}",
                                    crate::submit::BUSY_QUEUED_VERIFY_TIMEOUT_S
                                ));
                            }
                        }
                    }
                    None => {
                        // jsonl_path unresolvable on a BUSY send is anomalous: busy ⇒
                        // an EXISTING session ⇒ the transcript is resolvable;
                        // busy-queued and fresh-child are mutually exclusive (a fresh
                        // child is idle, not busy — design §4.3). Degrade to PENDING;
                        // do NOT reach for the fresh-child machinery here (keep the
                        // change self-scoped to the busy-queued path, constraint 4).
                        notes.push(format!(
                            "WARNING: could not resolve \"{label}\"'s transcript to verify the \
                             queued payload (the promise stays PENDING) — check: qd attach {label}"
                        ));
                    }
                }

                notes.extend(append_pty_invoked(
                    env,
                    clock,
                    &invoked_sid,
                    invoked_name.as_deref(),
                ));
                return Ok(PtyOutcome::Done(Delivered {
                    message_id: send_id,
                    stdout: Some(format!("Message queued in \"{label}\" (session busy)")),
                    notes,
                }));
            }
            // --wait falls through to the anchor loop (the anchor confirms uptake).
        }
        SendPtyAction::SendVerify => {
            // IDLE send — R4 TWO-WRITE delivery (text, ~200ms settle, separate "\r")
            // + acceptance-keyed CONTENT-VERIFIED verify-then-CR (ADR 0009; orc-2
            // RULED fix-in-phase, ruling relay-1780631655040-9 item 2). REPLACES the
            // single `message + "\r"` write (send.ts:204), which on REAL claude
            // 2.1.163 is paste-burst-absorbed at ≥~4KB and the remediation CR does
            // NOT recover it live (test/golden/dryrun/a4-live-evidence.md §FINDING +
            // a4-paste-bytes.txt; 2-boot repro). The verify (an idle session must go
            // busy) is PRESERVED — only the remediation CR is now content-verified
            // (fires only while the composer provably holds OUR text; never blind).
            // The single-write loss mechanism is TS-identical; this is a sanctioned
            // ADD-9a divergence (upstream report filed at merge).
            let mut verify_eligible = true;
            // §2.3.2 recorder acks for whichever idle sub-path runs (set in both).
            let idle_acks: ChunkAcks;
            // Set when outcome.accepted==false (pid-known path only); used for the
            // deferred QS-3 strict exit after chunks-delivered emits.
            let mut not_accepted = false;
            if let Some(pid) = session.pid.filter(|&p| p != 0) {
                // pid known → full two-write delivery + acceptance-verify. The
                // delivery drives a RECORDING IdleDeliverDeps (ACK-2 §9: same shape
                // as the production RealIdleDeliverDeps + deliver_idle_two_write,
                // but each text chunk's mux.send ack is captured). The pure
                // deliver_idle_two_write core + the IdleDeliverDeps trait are
                // UNTOUCHED — this is an impl swap only.
                let pid_file = paths.sessions_dir.join(format!("{pid}.json"));
                let rec_deps = RecordingIdleDeliverDeps {
                    mux,
                    clock,
                    sleeper,
                    zmx_name: zmx_name.clone(),
                    pid_file,
                    dir: op_dir.clone(),
                    total: Cell::new(0),
                    acked: Cell::new(0),
                };
                let outcome = deliver_idle_two_write(&rec_deps, message, SubmitOptions::default());
                idle_acks = rec_deps.acks();
                if !outcome.accepted {
                    // Not accepted (never went busy) → the W8 read-back has no
                    // submitted turn to verify; the existing WARNING carries it.
                    not_accepted = true;
                    verify_eligible = false;
                    if !wait {
                        // TS WARNING wording verbatim (send.ts:198-202).
                        notes.push(format!(
                            "WARNING: Message sent to \"{label}\" but session did not go busy \
                             — it may be stuck unsubmitted in the composer."
                        ));
                        // QS-3 strict exit deferred: taken AFTER chunks-delivered so the
                        // event stream is complete before we exit (symmetry with busy lane).
                    }
                }
            } else {
                // pid UNKNOWN → deliver but do NOT acceptance-verify (TS: the verify
                // is guarded by `session.pid`, send.ts:205). Still CHUNKED two-write
                // so a ≥~4KB payload lands (ADR 0009 mode (a) tty-queue overflow +
                // mode (b) paste \r absorption); the verify, not the delivery, needs
                // the pid. The W8 read-back still runs (it needs the transcript,
                // not the pid) — the only belt this branch has.
                let acks = send_text_chunked_mux(mux, sleeper, &op_dir, &zmx_name, message);
                sleeper.sleep_ms(crate::submit::TWO_WRITE_SETTLE_MS);
                let _ = mux.send(&op_dir, &zmx_name, "\r");
                idle_acks = acks;
            }

            // §2.3.2 chunks-delivered: all text chunks acked → emit (partial leaves
            // the send dangling by design; verb behavior unchanged).
            if idle_acks.all_ok() {
                events::warn_emit(
                    &writer,
                    clock,
                    &Payload::ChunksDelivered {
                        send_id: send_id.clone(),
                        chunks_acked: idle_acks.acked,
                        ack_source: ack_source_label(env).to_string(),
                    },
                );
            } else {
                // ADD-18 (ack3-spec §5.1): the idle send stage failed (any chunk's
                // mux.send returned Err). SHORT-CIRCUIT here: exit 11 loud, no W8
                // verify read-back, no --wait watch (a verify/watch on a failed write
                // is a guaranteed failure). send-initiated already emitted;
                // chunks-delivered correctly absent.
                //
                // §C2 (R5 seam ruling 01KX88WKGP): the STRUCTURALLY IDENTICAL
                // partial-write on the idle path. Same disposition as the busy arm
                // above — a partial write is in-band-undeterminable, so this door emits
                // NO terminal (a `pending-abandoned` here would false-fail an
                // ack-timeout-but-landed send and foreclose recovery). Fail loud (exit
                // 11) and leave the send dead-dangling; `qd delivery:recover` closes it
                // from the transcript.
                return Err(refuse(&mut notes, &send_id, idle_acks.write_failed()));
            }

            // QS-3 strict exit (idle lane, deferred from the not-accepted check above):
            // chunks-delivered has now been emitted (or the write-failed refusal
            // returned), so the event stream is complete before we exit. Strict mode
            // for unified send only; send:pty keeps its existing warning+exit-0
            // contract via strict=false. SILENT: the warning above is the account.
            if not_accepted && !wait && strict {
                return Err(refuse(&mut notes, &send_id, MuxPtyError::NotAccepted));
            }

            // W8 verify-after-submit (ADD-15, M11 sanctioned; ADR-0012): CHUNKED
            // idle-path deliveries get a bounded payload read-back BEFORE the
            // success print / --wait loop (fast-fail beats a 120s anchor timeout).
            // STRICT path-split (wart-wave-spec §4): the transcript pre-existed
            // here (resolved + offset-snapshotted pre-send), so Truncated AND
            // NoRecord are LOUD exit-1; Unattributable/SourceUnavailable degrade
            // to one warn; unresolvable-at-send degrades too (the send must not
            // gain a new precondition). Exit 1 = the existing failure class —
            // no new codes. NO auto-retry (double-submit risk, M11 §2).
            // §X.3.4/§X.5 (3-phase delivery) — UNGATE the W8 verify for the async
            // NO-WAIT path: a ≤1-chunk no-wait send is now verified too and emits
            // the uniform on-received `message-seen`. The `--wait` path is
            // UNTOUCHED — it runs the W8 verify only for >1-chunk sends
            // (`needs_verify`), exactly as before, and keeps `turn-anchored` (MF1).
            // So the ungate is scoped to `!wait` only; no `--wait` behavior changes.
            if verify_eligible && (needs_verify || !wait) {
                match &jsonl_path {
                    Some(jp) => {
                        let vdeps = SendPtyVerifyDeps {
                            jsonl_path: jp.clone(),
                            offset: start_offset,
                            clock,
                            sleeper,
                        };
                        match crate::submit::verify_chunked_payload(
                            &vdeps,
                            message,
                            crate::submit::VERIFY_TIMEOUT_S,
                            crate::submit::VERIFY_POLL_MS,
                        ) {
                            crate::submit::PayloadVerifyOutcome::Verified => {
                                if wait {
                                    // §9 --wait idle arm (MF1): KEEP `turn-anchored`.
                                    // The W8 verify emits THE landed signal and
                                    // verify_anchored suppresses the duplicate Complete
                                    // turn-anchored (m-1). recovered/attribution absent
                                    // (a LIVE anchor); line_index 0 (the verify helper
                                    // returns texts, not indices). UNCHANGED from today.
                                    emit_w8_anchored(
                                        &writer,
                                        clock,
                                        &send_id,
                                        message,
                                        jp,
                                        start_offset,
                                        0,
                                    );
                                    verify_anchored = true;
                                } else {
                                    // §X.3.4/§X.5 — async NO-WAIT W8 success → the
                                    // on-received `message-seen` (NOT turn-anchored, so
                                    // a consumer's reason=Seen gate can never be tripped by a
                                    // W8/--wait anchor). The call-base goal turn
                                    // (runner.rs:250, send_pty(.., false)) IS this path
                                    // → the consumer routes it to Fired{reason=Seen}.
                                    emit_w8_message_seen(&writer, clock, &send_id, message);
                                }
                            }
                            crate::submit::PayloadVerifyOutcome::Truncated {
                                expected,
                                recorded,
                            } => {
                                if needs_verify {
                                    // §2.3.4 / §9 W8 anchored-mismatch (terminal),
                                    // >1-chunk — the m5_* exit-1 failure terminal,
                                    // UNCHANGED. The PayloadVerifyOutcome carries
                                    // lengths only, so we re-read the user texts past
                                    // the offset at THIS call site and sha the longest
                                    // truncation-signature record for actual_sha (the
                                    // honest value; "" only if the re-read fails).
                                    // Emitted BEFORE the unchanged exit-1.
                                    emit_w8_mismatch(
                                        &writer,
                                        clock,
                                        &send_id,
                                        message,
                                        jp,
                                        start_offset,
                                        expected,
                                        recorded,
                                    );
                                    return Err(refuse(
                                        &mut notes,
                                        &send_id,
                                        MuxPtyError::PayloadTruncated {
                                            label,
                                            expected,
                                            recorded,
                                        },
                                    ));
                                }
                                // MF2 — a NEWLY-UNGATED ≤1-chunk no-wait send must NOT
                                // newly fail-loud (it exited 0 before the ungate).
                                // Degrade to a warn; emit NO terminal (a partial
                                // read-back of a short send is not a delivery failure —
                                // the promise stays PENDING, §X.6).
                                notes.push(format!(
                                    "WARNING: could not fully verify the short payload to \"{label}\" \
                                     (read-back truncated: expected {expected}, recorded {recorded}) — \
                                     check: qd attach {label}"
                                ));
                            }
                            crate::submit::PayloadVerifyOutcome::NoRecord => {
                                if needs_verify {
                                    // >1-chunk — the strict exit-1 failure, UNCHANGED.
                                    return Err(refuse(
                                        &mut notes,
                                        &send_id,
                                        MuxPtyError::PayloadNoRecord {
                                            label,
                                            timeout_s: crate::submit::VERIFY_TIMEOUT_S,
                                        },
                                    ));
                                }
                                // MF2 — ≤1-chunk no-wait: degrade-warn, NOT exit-1. No
                                // record within the W8 window ⇒ not-yet-seen ⇒ the
                                // promise stays PENDING (latency is normal, §X.6). A
                                // later landing is unobserved on the pty path (bounded
                                // W8) — pty's accepted reliability (§X.6).
                                notes.push(format!(
                                    "WARNING: could not verify short-payload arrival in \"{label}\"'s \
                                     transcript within {}s — check: qd attach {label}",
                                    crate::submit::VERIFY_TIMEOUT_S
                                ));
                            }
                            crate::submit::PayloadVerifyOutcome::Unattributable => {
                                notes.push(format!(
                                    "WARNING: could not attribute the delivered payload in \
                                     \"{label}\"'s transcript — check: qd attach {label}"
                                ));
                            }
                            crate::submit::PayloadVerifyOutcome::SourceUnavailable(why) => {
                                notes.push(format!(
                                    "WARNING: could not verify payload delivery to \"{label}\" \
                                     ({why}) — check: qd attach {label}"
                                ));
                            }
                        }
                    }
                    None => {
                        // §3.2 DISPATCH-PTY DEFERRED RESOLUTION — the S1 fresh-child
                        // fix. The transcript was unresolvable at send-time (a
                        // just-spawned child), so the old code only WARNED here and
                        // emitted no `message-seen` → the consumer's on-received gate never
                        // advanced → the priming chain truncated after turn-1
                        // (silent under-prime). Defer the resolution: poll the
                        // transcript into existence, anchor the content scan at the
                        // NIT-2 high-water floor (the first user-text record, past
                        // the leading init records — never byte 0), and on a unique
                        // exact-content match emit the SAME `message-seen` the
                        // resolvable path emits. `wait` is necessarily false here
                        // (the `--wait` hard precondition already refused above
                        // when the path was unresolvable), so this is the no-wait
                        // on-received path only; on miss it degrades to a VISIBLE
                        // PENDING stall, never a wrong-fire (symmetric with the
                        // NoRecord no-wait degrade).
                        let ddeps = DeferredFreshChildDeps {
                            projects_dir: paths.projects_dir.clone(),
                            session_id: session.session_id.clone(),
                            clock,
                            sleeper,
                        };
                        match crate::submit::deferred_verify_fresh_child(
                            &ddeps,
                            message,
                            crate::submit::VERIFY_TIMEOUT_S,
                            crate::submit::VERIFY_POLL_MS,
                        ) {
                            crate::submit::PayloadVerifyOutcome::Verified => {
                                // The fresh child's no-wait pty `message-seen`
                                // fires (the S1 stall is closed); the consumer's OnReceived
                                // gate (joining by send_id) fires link N+1.
                                emit_w8_message_seen(&writer, clock, &send_id, message);
                            }
                            _ => {
                                notes.push(format!(
                                    "WARNING: could not verify payload delivery to \"{label}\" \
                                     (fresh-child transcript did not resolve/anchor within the \
                                     deferred window — the promise stays PENDING) — check: \
                                     qd attach {label}"
                                ));
                            }
                        }
                    }
                }
            }

            if !wait {
                notes.extend(append_pty_invoked(
                    env,
                    clock,
                    &invoked_sid,
                    invoked_name.as_deref(),
                ));
                return Ok(PtyOutcome::Done(Delivered {
                    message_id: send_id,
                    stdout: Some(format!("Message sent to {label}")),
                    notes,
                }));
            }
        }
    }

    // --- the --wait JSONL anchor loop is the CALLER's (send.ts:208-265) ------
    // Both zmx paths converge here with `wait == true`. Everything the loop needs
    // was resolved above; everything it DOES — the mid-line "Waiting for response"
    // banner, the per-poll progress glyph, the WatchGuard, and the arm-by-arm
    // reply capture — is inseparable from printing, so it stayed in the verb.
    Ok(PtyOutcome::Await(PtyAwait {
        notes,
        phase: WaitPhase::AnchorLoop,
        send_id,
        label,
        jsonl_path,
        start_offset,
        verify_anchored,
        paths,
        ev_paths,
    }))
}

// ===========================================================================
// The ledger emitters — qw-owned payloads
// ===========================================================================

/// §X.3.4 (3-phase delivery, on-received) — the async NO-WAIT W8 success signal.
/// A Verified read-back on the async no-wait path IS the recipient pulling the
/// message into working context. Deliberately `message-seen` (a NEW terminal kind,
/// NOT `turn-anchored`) so the consumer's on-received reason=Seen gate is satisfied and no
/// `--wait`/W8 `turn-anchored` anchor can ever false-fire it (§X.5).
/// `content_sha256 = sha256(message)` — the robust pty key (the same bytes the
/// sender hashed). Carries no prose (§X.7).
pub fn emit_w8_message_seen(writer: &EventWriter, clock: &dyn Clock, send_id: &str, message: &str) {
    events::warn_emit(
        writer,
        clock,
        &Payload::MessageSeen {
            send_id: send_id.to_string(),
            content_sha256: events::sha256_hex(message.as_bytes()),
        },
    );
}

/// §9 `turn-anchored` emission — the landed signal, from all THREE of its sites.
///
/// This carrier's idle-path W8 verify and [`super::priming`]'s (`line_index` 0 on
/// both: the verify helper returns texts, not indices — a documented unknown), and
/// [`run_anchor_wait`]'s Complete arm (`line_index` the producer's own sticky
/// index) build the SAME payload from the same offset, so they share one
/// constructor. `recovered` false and `attribution` absent on all three: this is a
/// LIVE anchor, never a recovery.
///
/// An UNRESOLVED transcript is `Path::new("")`, whose `display()` is the empty
/// string the pre-move `-p` emitter wrote from its `Option`. That is why this takes
/// a `&Path` rather than an `Option<&Path>`: the empty path already means "no
/// transcript" in the record, and a second spelling of it would be a second thing
/// to keep in agreement.
pub fn emit_w8_anchored(
    writer: &EventWriter,
    clock: &dyn Clock,
    send_id: &str,
    message: &str,
    transcript: &Path,
    offset: u64,
    line_index: u64,
) {
    events::warn_emit(
        writer,
        clock,
        &Payload::TurnAnchored {
            send_id: send_id.to_string(),
            content_sha256: events::sha256_hex(message.as_bytes()),
            anchor: Anchor {
                transcript: transcript.display().to_string(),
                start_offset: offset,
                line_index,
            },
            recovered: false,
            attribution: None,
        },
    );
}

/// §2.3.4 / §9 W8 anchored-mismatch emission (idle path Truncated), shared with
/// [`super::priming`]'s verify arm. The PayloadVerifyOutcome carries lengths only;
/// we re-read the user texts past the offset HERE and sha the longest
/// truncation-signature record for `actual_sha` (the honest value). On a re-read
/// failure `actual_sha` is "" (named gap), never invented — which is also what an
/// unresolved transcript (`Path::new("")`, unreadable) produces, exactly as the
/// pre-move `-p` emitter's `Option` did. `expected_sha` = sha256(message).
#[allow(clippy::too_many_arguments)]
pub fn emit_w8_mismatch(
    writer: &EventWriter,
    clock: &dyn Clock,
    send_id: &str,
    message: &str,
    transcript: &Path,
    offset: u64,
    expected: usize,
    recorded: usize,
) {
    let actual_sha = crate::submit::read_user_texts_past_offset(transcript, offset)
        .ok()
        .and_then(|texts| {
            crate::submit::longest_truncation_signature(&texts, message)
                .map(|r| events::sha256_hex(r.as_bytes()))
        })
        .unwrap_or_default();
    events::warn_emit(
        writer,
        clock,
        &Payload::TurnAnchoredMismatch {
            send_id: send_id.to_string(),
            expected_sha: events::sha256_hex(message.as_bytes()),
            actual_sha,
            expected_len: expected as u64,
            actual_len: recorded as u64,
            recovered: false,
            attribution: None,
        },
    );
}

/// §2.3.9 `status-transition` emission, source `status-file-poll`.
///
/// Only the `--wait` loop's deps impl calls this (red-team R4: the seam is the
/// DEPS IMPL, never `run_wait_loop`'s pure core). It is `pub` because that impl
/// used to live in the `qd` verb; it is now [`RealWaitDeps`] a few hundred lines
/// below, and the visibility is kept rather than narrowed so the qw-side unit rows
/// can drive the emitter directly.
pub fn emit_status_transition(writer: &EventWriter, clock: &dyn Clock, status: &str) {
    events::warn_emit(
        writer,
        clock,
        &Payload::StatusTransition {
            status: status.to_string(),
            source: "status-file-poll".to_string(),
        },
    );
}

// ===========================================================================
// The verify deps
// ===========================================================================

/// W8 [`crate::submit::VerifyDeps`] for the send:pty idle path: the transcript was
/// resolved + offset-snapshotted PRE-send, so each poll is a direct read past
/// that offset (the shared `read_user_texts_past_offset` helper — same
/// shrink/boundary loud-degrade semantics as the --wait loop's reader).
struct SendPtyVerifyDeps<'a> {
    jsonl_path: PathBuf,
    offset: u64,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl crate::submit::VerifyDeps for SendPtyVerifyDeps<'_> {
    fn read_user_texts(&self) -> Result<Vec<String>, String> {
        crate::submit::read_user_texts_past_offset(&self.jsonl_path, self.offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// W8 [`crate::submit::DeferredVerifyDeps`] for the FRESH-CHILD no-wait pty path
/// (RESPEC-DELTA §3.2): the transcript was UNRESOLVABLE pre-send, so resolution is
/// deferred. `resolve_path` re-runs the SAME `find_jsonl_path(projects_dir,
/// session_id, None)` the relay observer polls; the high-water floor + read reuse
/// the existing `first_user_text_offset` / `read_user_texts_past_offset` helpers.
struct DeferredFreshChildDeps<'a> {
    projects_dir: PathBuf,
    session_id: String,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl crate::submit::DeferredVerifyDeps for DeferredFreshChildDeps<'_> {
    fn resolve_path(&self) -> Option<PathBuf> {
        crate::jsonl::find_jsonl_path(&self.projects_dir, &self.session_id, None)
    }
    fn first_user_offset(&self, path: &Path) -> Option<u64> {
        crate::submit::first_user_text_offset(path)
    }
    fn read_user_texts(&self, path: &Path, offset: u64) -> Result<Vec<String>, String> {
        crate::submit::read_user_texts_past_offset(path, offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// W8 [`crate::submit::DeferredVerifyDeps`] for the §7-B BUSY-QUEUED no-wait pty
/// path (PTY-FIX-DESIGN.md §4) — the busy-queued analogue of
/// [`DeferredFreshChildDeps`]. The recipient is an EXISTING session whose
/// transcript was already resolved pre-send (`resolve_path` just hands it back; no
/// poll-into-existence), and the high-water floor is the CAPTURED PRE-SEND OFFSET
/// (`pre_send_offset` = the idle path's `start_offset`, snapshotted before typing,
/// PAST the prior in-flight turn's user record — constraint 1), NOT the first
/// user-text record. `read_user_texts` reuses `read_user_texts_past_offset`
/// verbatim, so the uniqueness/high-water/PENDING policy is shared byte-for-byte
/// with the fresh-child path via [`crate::submit::deferred_verify_busy_queued`].
struct DeferredBusyQueuedDeps<'a> {
    jsonl_path: PathBuf,
    pre_send_offset: u64,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl crate::submit::DeferredVerifyDeps for DeferredBusyQueuedDeps<'_> {
    fn resolve_path(&self) -> Option<PathBuf> {
        // Already resolved pre-send (existing busy session) — no deferred
        // resolution needed, unlike the fresh-child path.
        Some(self.jsonl_path.clone())
    }
    fn first_user_offset(&self, _path: &Path) -> Option<u64> {
        // Constraint 1: the busy-queued floor is the captured pre-send offset
        // (independent of `path`), so the scan anchors past the prior in-flight
        // turn's record and can never re-anchor on an earlier identical body.
        Some(self.pre_send_offset)
    }
    fn read_user_texts(&self, path: &Path, offset: u64) -> Result<Vec<String>, String> {
        crate::submit::read_user_texts_past_offset(path, offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}
// ===========================================================================
// The `--wait` reply-capture half
// ===========================================================================
//
// The other side of the `wait`/`raw`/`full` cut, moved here after the carrier.
// Phase 3B left it in `bin/qd/verbs/send.rs` because its printing is not
// line-shaped — `eprint!("Waiting for response")` opens a stderr line, a glyph is
// written into it every 500ms, and one of five arms closes it 120 seconds later —
// and a [`Notes`] entry is a whole line.
//
// That reasoning was about the LINE, and it still holds: the banner and all five
// closing arms stay in the verb, which is why [`ReplyOutcome`] carries no text a
// caller has to render. What moved is everything the loop WRITES: the
// [`EventWriter`] it rebuilt from `PtyAwait::ev_paths` to close the send it armed,
// the `status-transition` emitter wired into its status read, its `WatchGuard`,
// and the `turn-anchored` its Complete arm mints. Those are qw-owned ledger
// records that were being written from a qd verb — the last of them.
//
// The one thing that still prints from here is the per-poll progress GLYPH, and
// it is a character rather than a line: it cannot be returned and then written,
// because its whole meaning is that it appears WHILE the loop runs. It goes to
// stderr, which `11-stage3-plan.md` D5 keeps inherited across the process cut for
// exactly this reason — the same standing [`super::render`] has.

/// [`WatchGuard`] is generic over a SIZED [`Clock`], because its unit rows drive
/// it with a `FixedClock` while the verb layer passes a `RealClock`. This carrier
/// holds `&dyn Clock`, which is neither, so it hands the guard a sized shim rather
/// than widening the guard's bound — an unsized `C` there would stop `warn_emit`'s
/// `&dyn Clock` coercion from compiling, which is a worse trade for one call site.
struct ClockRef<'a>(&'a dyn Clock);

impl Clock for ClockRef<'_> {
    fn now_ms(&self) -> i64 {
        self.0.now_ms()
    }
}

/// The owned inputs of one `--wait` reply capture.
///
/// Everything the loop RESOLVED is on [`PtyAwait`] already and is not repeated
/// here; what this adds is the four things the delivery half never saw, because
/// all four belong to flags it does not read: the `--timeout` bound, the
/// `--raw`/`--full` [`ExtractMode`], the message to anchor on, and the row.
pub struct ReplyParams<'a> {
    pub session: &'a Session,
    pub message: &'a str,
    /// The transcript to anchor in. Handed in rather than read off
    /// [`PtyAwait::jsonl_path`] because the caller has already had to resolve the
    /// `None` case — under [`WaitPhase::EmbeddedTerminal`] a missing transcript is
    /// a confirmed delivery with an uncaptured reply, which is the caller's
    /// sentence and its exit 0.
    pub jsonl_path: &'a Path,
    /// The `--timeout` bound in ms, already parsed by [`timeout_ms`].
    pub timeout_ms: i64,
    pub mode: ExtractMode,
}

/// What the reply capture observed. **Five arms of stderr and one of stdout are
/// the CALLER's** — this type says only which one.
///
/// `Complete` carries `capture_or_defect`'s verdict rather than the raw lines: the
/// extraction is pure over lines already in memory, so running it here moves no
/// I/O and leaves the caller with nothing to re-derive.
pub enum ReplyOutcome {
    /// The response turn completed. `Ok(body)` is the caller's stdout;
    /// `Err(observed)` is the loud empty-capture exit.
    Complete { capture: Result<String, String> },
    /// The pid status became unreadable mid-wait.
    Died,
    /// The `--timeout` elapsed. `anchored` picks between the caller's two wordings.
    TimedOut { anchored: bool },
    /// The transcript lost its integrity mid-wait (ADD-8 W5).
    SourceError(String),
}

/// The ZMX `--wait` JSONL anchor loop — the phase that WRITES.
///
/// The writer is rebuilt from [`PtyAwait::ev_paths`], which is the same derivation
/// the delivery half wrote its `send-initiated` through: the `--wait` terminal has
/// to land in the file that holds the record it closes, and re-deriving the path
/// here is how the two could disagree.
///
/// **D8 lives on this function.** Only the `Complete` arm emits, and only when the
/// W8 verify has not already anchored this send (`m-1`: one landed signal per
/// `send_id`). `Died`, `TimedOut` and `SourceError` write NOTHING — each of the
/// three is a possibly-LANDED send whose fate this observer cannot see, and a
/// terminal minted on any of them would foreclose, under first-terminal-wins, the
/// recovery read `qd delivery:recover` exists to run. That is why the disarm below
/// is unconditional: it is what stops the guard's `Drop` from re-minting on
/// exactly those three paths. A budget belongs to the caller; the ledger belongs
/// to the session.
pub fn run_anchor_wait(deps: &PtyDeps, a: &PtyAwait, p: &ReplyParams) -> ReplyOutcome {
    let writer = EventWriter::for_key(
        &a.ev_paths.state_dir,
        &p.session.session_id,
        Some(p.session.session_id.clone()),
        p.session.name.clone(),
    );

    // §9 status-transition seam (red-team R4): the emission lives in the DEPS
    // IMPL, never `run_wait_loop`'s pure core. [`RealWaitDeps::read_status`] wraps
    // `read_pid_status` with a last-status Cell + an emitter; the pure core and
    // the `WaitDeps` trait stay intact. The emitter borrows the writer/clock; the
    // loop is the only caller, so a RefCell-free Cell suffices.
    let last_status: Cell<Option<String>> = Cell::new(None);
    let wait_deps = RealWaitDeps {
        jsonl_path: p.jsonl_path.to_path_buf(),
        start_offset: a.start_offset,
        pid_file: wait_pid_file(p.session, &a.paths),
        clock: deps.clock,
        sleeper: deps.sleeper,
        status_emit: Some(StatusEmit {
            writer: &writer,
            clock: deps.clock,
            last: &last_status,
        }),
    };

    // §9 / rev C row 24 WatchGuard: armed for the duration of the `--wait` watch
    // as a panic/early-return safety net — an unwind that skips the disarm below
    // Drops it → pending-abandoned{watch-interrupted}. SIGKILL bypasses Drop and
    // is covered by the reader-side dead-writer rule (§7).
    let clock_ref = ClockRef(deps.clock);
    let guard = WatchGuard::arm(&writer, &clock_ref, &a.send_id);

    let outcome = run_wait_loop(&wait_deps, p.message, p.timeout_ms, 500);
    let reply = match outcome {
        WaitOutcome::Complete { lines, anchor } => {
            // §9: --wait Complete → turn-anchored (terminal). The anchor index is
            // the producer's own (sticky); start_offset is the pre-send snapshot.
            // m-1: SKIPPED when the W8 verify already anchored this send — one
            // landed signal per send_id, never a systematic duplicate.
            if !a.verify_anchored {
                emit_w8_anchored(
                    &writer,
                    deps.clock,
                    &a.send_id,
                    p.message,
                    p.jsonl_path,
                    a.start_offset,
                    anchor.unwrap_or(0) as u64,
                );
            }
            ReplyOutcome::Complete {
                capture: capture_or_defect(&lines, anchor, p.mode),
            }
        }
        WaitOutcome::Died => ReplyOutcome::Died,
        WaitOutcome::TimedOut { anchored } => ReplyOutcome::TimedOut { anchored },
        WaitOutcome::SourceError(reason) => ReplyOutcome::SourceError(reason),
    };
    guard.disarm();
    reply
}

/// The EMBEDDED backend's reply capture — the phase that writes NOTHING.
///
/// Reached only after `LaneOps::await_terminal` already answered `Terminal::Seen`,
/// so the send's one terminal is the MUX's and this loop exists purely to read the
/// reply back. No writer, no `WatchGuard`, no status-transition emitter: the
/// single-writer split is what makes the terminal above trustworthy, and a second
/// writer here would be the thing that breaks it.
pub fn run_reply_capture(deps: &PtyDeps, a: &PtyAwait, p: &ReplyParams) -> ReplyOutcome {
    let wait_deps = RealWaitDeps {
        jsonl_path: p.jsonl_path.to_path_buf(),
        start_offset: a.start_offset,
        pid_file: wait_pid_file(p.session, &a.paths),
        clock: deps.clock,
        sleeper: deps.sleeper,
        status_emit: None,
    };
    match run_wait_loop(&wait_deps, p.message, p.timeout_ms, 500) {
        WaitOutcome::Complete { lines, anchor } => ReplyOutcome::Complete {
            capture: capture_or_defect(&lines, anchor, p.mode),
        },
        WaitOutcome::Died => ReplyOutcome::Died,
        WaitOutcome::TimedOut { anchored } => ReplyOutcome::TimedOut { anchored },
        WaitOutcome::SourceError(reason) => ReplyOutcome::SourceError(reason),
    }
}

/// The `--wait` pid file for the status read inside the loop. `None` → no pid
/// known.
fn wait_pid_file(session: &Session, paths: &QdPaths) -> Option<PathBuf> {
    session
        .pid
        .filter(|&p| p != 0)
        .map(|pid| paths.sessions_dir.join(format!("{pid}.json")))
}

/// §9 status-transition emitter handle wired into [`RealWaitDeps::read_status`]
/// (red-team R4: the seam is the DEPS IMPL, never the pure `run_wait_loop`). On a
/// CHANGE (incl. the first observation) it emits `status-transition` with
/// source="status-file-poll". Fidelity = one observation per poll (500ms),
/// exactly what the loop itself sees (fast flips between polls are aliased away —
/// documented in §2.3.9).
struct StatusEmit<'a> {
    writer: &'a EventWriter,
    clock: &'a dyn Clock,
    last: &'a Cell<Option<String>>,
}

impl StatusEmit<'_> {
    fn observe(&self, status: &str) {
        // Cell::take + restore to compare without requiring Clone-on-read.
        let prev = self.last.take();
        let changed = prev.as_deref() != Some(status);
        self.last.set(Some(status.to_string()));
        if changed {
            emit_status_transition(self.writer, self.clock, status);
        }
    }
}

struct RealWaitDeps<'a> {
    jsonl_path: PathBuf,
    start_offset: u64,
    /// `None` → no pid known: the status read can never succeed, so the loop
    /// treats every poll as "died" (TS would `JSON.parse(undefined)` → throw →
    /// the catch → died; we surface the same outcome immediately).
    pid_file: Option<PathBuf>,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
    /// §9 status-transition emitter (None on the embedded path, which writes
    /// nothing, and in the pure unit tests of the loop).
    status_emit: Option<StatusEmit<'a>>,
}

impl WaitDeps for RealWaitDeps<'_> {
    fn read_lines(&self) -> Result<Vec<ParsedLine>, String> {
        // Read the whole file, slice past start_offset, parse once (N10).
        let content = std::fs::read_to_string(&self.jsonl_path).unwrap_or_default();
        let off = self.start_offset as usize;
        // ADD-8 W5: a file SHORTER than the pre-send offset means it was
        // truncated/rotated mid-wait. NEVER fall back to slicing from byte 0 —
        // an exact-text re-scan could anchor on an EARLIER identical message
        // (silent wrong-anchor). Fail loud instead. (An unreadable file reads as
        // "" which also trips this once off > 0 — same integrity loss.)
        if content.len() < off {
            return Err(format!(
                "conversation JSONL shrank below the start offset ({} < {off} bytes) — \
                 rotated/truncated while waiting",
                content.len()
            ));
        }
        let slice = content.get(off..).ok_or_else(|| {
            // len >= off but not a char boundary: the file was REWRITTEN with
            // different bytes around the offset — same integrity loss.
            format!(
                "start offset {off} no longer falls on a char boundary — conversation \
                     JSONL was rewritten while waiting"
            )
        })?;
        Ok(parse_jsonl_slice(slice))
    }
    fn read_status(&self) -> Option<String> {
        // No pid → unreadable (None) → loop reports Died (parity with the TS
        // pid-file read catch).
        let status = self.pid_file.as_ref().and_then(|p| read_pid_status(p));
        // §9 status-transition: emit on each observed change (incl. first). The
        // seam is HERE (the deps impl), never the pure run_wait_loop core (R4).
        if let (Some(emit), Some(s)) = (self.status_emit.as_ref(), status.as_deref()) {
            emit.observe(s);
        }
        status
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
    fn progress(&self, glyph: char) {
        use std::io::Write;
        let mut err = std::io::stderr();
        let _ = write!(err, "{glyph}");
        let _ = err.flush();
    }
}

#[cfg(test)]
mod tests {
    //! These eleven followed their subjects here from `bin/qd/verbs/send.rs`
    //! (stage-3 phase 3B). Nothing about what they assert changed; what changed is
    //! that `ack_source_label` now takes its `Env` instead of reading the process,
    //! that the write-failure line and exit code are read off [`MuxPtyError`]
    //! rather than off a printing `write_failed_exit`, and that the two source
    //! scans read THIS file.
    use super::*;
    use crate::effects::MapEnv;
    use crate::exec::ExecResult;
    use crate::mux::MuxSession;
    use std::io;

    // A minimal mux whose `send` succeeds or fails on demand — the seam G3 needs
    // to prove chunks-delivered fires ONLY when every text chunk acked. Only
    // `send`/`history` carry meaning; the rest are unreachable on these paths.
    struct ProbeMux {
        send_ok: bool,
    }
    impl Mux for ProbeMux {
        fn list(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
            Ok(vec![])
        }
        fn list_raw(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
            Ok(vec![])
        }
        fn run_detached(&self, _d: &Path, _n: &str, _c: &str, _w: &Path) -> io::Result<ExecResult> {
            unreachable!()
        }
        fn send(&self, _d: &Path, _n: &str, _t: &str) -> io::Result<ExecResult> {
            if self.send_ok {
                Ok(ExecResult {
                    status: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                })
            } else {
                Err(io::Error::other("send failed"))
            }
        }
        fn kill(&self, _d: &Path, _n: &str) -> io::Result<i32> {
            Ok(0)
        }
        fn history(&self, _d: &Path, _n: &str) -> io::Result<String> {
            Ok(String::new())
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> io::Result<i32> {
            Ok(0)
        }
        fn attach(&self, _d: &Path, _n: &str) -> io::Result<i32> {
            Ok(0)
        }
    }

    struct NoSleep;
    impl Sleeper for NoSleep {
        fn sleep_ms(&self, _ms: u64) {}
    }

    // ADD-18 partial-ack probe: succeeds for the FIRST `ok_count` text sends, then
    // Errs — drives the "1 of 2 chunks acked" partial-failure row (§5.2).
    struct PartialMux {
        ok_count: Cell<u32>,
    }
    impl Mux for PartialMux {
        fn list(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
            Ok(vec![])
        }
        fn list_raw(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
            Ok(vec![])
        }
        fn run_detached(&self, _d: &Path, _n: &str, _c: &str, _w: &Path) -> io::Result<ExecResult> {
            unreachable!()
        }
        fn send(&self, _d: &Path, _n: &str, _t: &str) -> io::Result<ExecResult> {
            let remaining = self.ok_count.get();
            if remaining > 0 {
                self.ok_count.set(remaining - 1);
                Ok(ExecResult {
                    status: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: false,
                })
            } else {
                Err(io::Error::other("send failed"))
            }
        }
        fn kill(&self, _d: &Path, _n: &str) -> io::Result<i32> {
            Ok(0)
        }
        fn history(&self, _d: &Path, _n: &str) -> io::Result<String> {
            Ok(String::new())
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> io::Result<i32> {
            Ok(0)
        }
        fn attach(&self, _d: &Path, _n: &str) -> io::Result<i32> {
            Ok(0)
        }
    }

    /// A 2-chunk message (>1024 UTF-8 bytes): the production splitter yields 2.
    fn two_chunk_msg() -> String {
        "a".repeat(2000)
    }

    #[test]
    fn recorder_all_ok_when_every_send_succeeds() {
        let mux = ProbeMux { send_ok: true };
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", &two_chunk_msg());
        assert_eq!(acks.total, 2);
        assert_eq!(acks.acked, 2);
        // all_ok ⇒ chunks-delivered WOULD emit (the §9 gate).
        assert!(acks.all_ok());
    }

    #[test]
    fn recorder_not_all_ok_when_a_send_fails_so_no_chunks_delivered() {
        let mux = ProbeMux { send_ok: false };
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", &two_chunk_msg());
        assert_eq!(acks.total, 2);
        assert_eq!(acks.acked, 0);
        // A partial send leaves the send DANGLING by design — NO chunks-delivered.
        // This is the structural guard: chunks-delivered keys on all_ok ONLY.
        assert!(!acks.all_ok());
    }

    #[test]
    fn recorder_empty_message_is_not_all_ok() {
        let mux = ProbeMux { send_ok: true };
        // chunk_text("") == [] → zero chunks → never "delivered".
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", "");
        assert_eq!(acks.total, 0);
        assert!(!acks.all_ok());
    }

    /// An `Env` with the given `QD_MUX`, or none at all.
    fn mux_env(qd_mux: Option<&str>) -> MapEnv {
        let mut vars = std::collections::HashMap::new();
        if let Some(v) = qd_mux {
            vars.insert("QD_MUX".to_string(), v.to_string());
        }
        MapEnv { vars, uid: 501 }
    }

    #[test]
    fn ack_source_label_is_one_of_the_two_named_channels() {
        // §2.3.2: the channel name is honest — exactly input-sent | cli-exit.
        for qd_mux in [None, Some("zmx"), Some("embedded"), Some("nonsense")] {
            let label = ack_source_label(&mux_env(qd_mux));
            assert!(
                matches!(label, "input-sent" | "cli-exit"),
                "{qd_mux:?} named {label}"
            );
        }
        // And the two are keyed off the SELECTED backend, not guessed: zmx
        // observes only the process exit; everything else (including the
        // unparseable value, which falls back to the C1 default) blocks on the
        // daemon's per-write ack.
        assert_eq!(ack_source_label(&mux_env(Some("zmx"))), "cli-exit");
        assert_eq!(ack_source_label(&mux_env(None)), "input-sent");
        assert_eq!(ack_source_label(&mux_env(Some("nonsense"))), "input-sent");
    }

    #[test]
    fn recording_idle_deps_count_only_text_not_cr() {
        // The idle recorder counts send_text chunks; send_cr (the "\r") is NOT a
        // chunks-delivered ack (only text chunks are evidence).
        let mux = ProbeMux { send_ok: true };
        let clock = crate::effects::RealClock;
        let deps = RecordingIdleDeliverDeps {
            mux: &mux,
            clock: &clock,
            sleeper: &NoSleep,
            zmx_name: "wk".to_string(),
            pid_file: PathBuf::from("/nope.json"),
            dir: PathBuf::from("/d"),
            total: Cell::new(0),
            acked: Cell::new(0),
        };
        deps.send_text("hello");
        deps.send_cr(); // must NOT bump the tally.
        deps.send_text("world");
        let acks = deps.acks();
        assert_eq!(acks.total, 2);
        assert_eq!(acks.acked, 2);
    }

    // -----------------------------------------------------------------------
    // ADD-18 (ack3-spec §5): send:pty WRITE-FAILED exit contract (bin-unit).
    //
    // The full verb-level surface (exit 11 + stderr line over a REAL fault-armed
    // daemon, the N-twin clean-path exit 0, and the --wait short-circuit through
    // the real binary) lands in W3's ack3_matrix.rs::add18_write_failure_exit_
    // contract (e2e, §5.2). These rows pin the in-process DECISION the verb makes
    // off the recorder tally — the seam `run_send_pty` branches on.
    // -----------------------------------------------------------------------

    #[test]
    fn add18_partial_ack_one_of_two_is_write_failed_exit_11() {
        // (a) §5.2 partial-ack rule: 1 of 2 chunks acked is FAILURE (message
        // integrity is all-or-nothing). The verb's else-branch fires write_failed.
        let mux = PartialMux {
            ok_count: Cell::new(1),
        };
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", &two_chunk_msg());
        assert_eq!(acks.total, 2);
        assert_eq!(acks.acked, 1);
        assert!(!acks.all_ok(), "partial ack is NOT all_ok → write-failed");
        let refusal = acks.write_failed();
        assert_eq!(refusal.exit_code(), 11);
        // The EXACT stderr line (§5.1 wording, pinned by the contract). The verb
        // is ignored — this line has never been verb-attributed.
        assert_eq!(
            refusal.line("send:pty").as_deref(),
            Some("ERROR: PTY write failed (1/2 chunks acked) — see events file")
        );
    }

    #[test]
    fn add18_all_chunks_err_is_write_failed_exit_11() {
        // (b) every chunk errs → 0/2 acked → exit 11 + the exact line.
        let mux = ProbeMux { send_ok: false };
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", &two_chunk_msg());
        assert_eq!(acks.acked, 0);
        assert!(!acks.all_ok());
        let refusal = acks.write_failed();
        assert_eq!(refusal.exit_code(), 11);
        assert_eq!(
            refusal.line("send:pty").as_deref(),
            Some("ERROR: PTY write failed (0/2 chunks acked) — see events file")
        );
    }

    #[test]
    fn add18_clean_send_is_not_write_failed() {
        // (c) clean send: all_ok ⇒ the verb takes the chunks-delivered + success
        // path, NEVER write_failed (exit 0 stdout "Message queued"/"Message sent…"
        // is verb-level, asserted in the e2e N-twin). Here: the decision gate.
        let mux = ProbeMux { send_ok: true };
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", &two_chunk_msg());
        assert!(acks.all_ok(), "clean send is all_ok → NOT write-failed");
    }

    #[test]
    fn add18_exit_code_is_11_distinct_from_1_and_10() {
        // The §5.1 exit-code triple: 11 is the write-failed code, distinct from the
        // generic-1 / new-p-Stalled-10 surfaces (machine-readable). A regression
        // that collapses it onto 1 or 10 REDs here.
        assert_eq!(EXIT_PTY_WRITE_FAILED, 11);
    }

    // QS-3 strict-mode structural guard: [`send_mux_pty`] MUST refuse in both the
    // "stuck" (busy-lane, still in composer) and "not accepted" (idle-lane, never
    // went busy) paths when `strict=true`. The explicit `send:pty` verb passes
    // `strict=false` and preserves its warning+continue contract; every carrier
    // caller passes `strict=true` so `qd send` cannot exit 0 on a known-failed
    // submission. Both refusals are SILENT (`line` is `None`) because their warning
    // was already written as a note — asserting the variant rather than a bare
    // `return 1` is what makes that visible here.
    // Structural form because exercising these paths via unit test requires a real
    // live PTY session; the source guard is the regression tripwire.
    //
    // Implementation note: the idle-lane strict exit is DEFERRED to after the
    // chunks-delivered emit (event stream symmetry with the busy lane). The test
    // verifies the deferred compound check `!outcome.accepted && !wait && strict`.
    //
    // MUTATION EVIDENCE: removing either strict guard reds this.
    #[test]
    fn unified_pty_strict_mode_exits_nonzero_on_stuck_and_unaccepted_submissions() {
        let src = include_str!("pty.rs");
        let fn_start = src
            .find("pub fn send_mux_pty(")
            .expect("send_mux_pty must exist");
        let after_fn = &src[fn_start..];
        // Scope to the function body: emit_w8_message_seen immediately follows.
        let fn_end = after_fn
            .find("\npub fn emit_w8_message_seen(")
            .expect("emit_w8_message_seen must follow send_mux_pty");
        let body = &after_fn[..fn_end];

        // --- stuck path (busy lane) ---
        // Positive control: warning is present.
        assert!(
            body.contains("message may be stuck unsubmitted"),
            "stuck warning must still exist in send_mux_pty"
        );
        // The stuck guard fires immediately after the warning (within the `if !wait` branch).
        let stuck_warn_pos = body.find("message may be stuck unsubmitted").unwrap();
        let stuck_region = &body[stuck_warn_pos..(stuck_warn_pos + 600).min(body.len())];
        assert!(
            stuck_region.contains("if strict {"),
            "send_mux_pty must guard the stuck path with `if strict {{` (QS-3). \
             Region after warning:\n{stuck_region}"
        );
        assert!(
            stuck_region.contains("MuxPtyError::StuckComposer"),
            "send_mux_pty must refuse with StuckComposer in the strict stuck path \
             (QS-3). Region after warning:\n{stuck_region}"
        );

        // --- not-accepted path (idle lane, deferred check) ---
        // Positive control: warning is present.
        assert!(
            body.contains("did not go busy"),
            "not-accepted warning must still exist in send_mux_pty"
        );
        // The idle-lane strict exit is DEFERRED to after chunks-delivered for event
        // symmetry. `not_accepted` captures outcome.accepted==false from the pid-known
        // branch so it remains visible outside the if-let scope.
        // Pin the deferred compound check (the whole expression).
        assert!(
            body.contains("not_accepted && !wait && strict"),
            "send_mux_pty must have the deferred idle-lane strict check \
             `not_accepted && !wait && strict` (QS-3, idle-lane guard after \
             chunks-delivered emit). Body does not contain the guard."
        );
        // Confirm return 1 follows it (within a tight window).
        let deferred_pos = body.find("not_accepted && !wait && strict").unwrap();
        let deferred_region = &body[deferred_pos..(deferred_pos + 140).min(body.len())];
        assert!(
            deferred_region.contains("MuxPtyError::NotAccepted"),
            "the deferred idle-lane strict check must refuse with NotAccepted \
             (QS-3). Region:\n{deferred_region}"
        );
    }

    // QS-3 wiring guard (F-1 Fable finding): pins that the CARRIER entry wires
    // `strict=true`. A one-token regression (`true`→`false`) would silently
    // disconnect QS-3 from `qd send` while all other tests stay green.
    //
    // Its subject MOVED with the body. It used to scan
    // `send::run_send_pty_unified`, the `Carriers` forward; that function is gone
    // with the trait, and the one entry every lane arm now reaches is
    // [`deliver_mux_pty`]. Structural form for the same reason as before: the call
    // site is the only wiring and no mock can exercise it.
    // MUTATION EVIDENCE: `strict: true`→`false` in `deliver_mux_pty` reds this.
    #[test]
    fn unified_pty_entry_wires_strict_true() {
        let src = include_str!("pty.rs");
        let fn_start = src
            .find("pub fn deliver_mux_pty(")
            .expect("deliver_mux_pty must exist");
        let after_fn = &src[fn_start..];
        // Scope to the function body: send_mux_pty immediately follows.
        let fn_end = after_fn
            .find("\npub fn send_mux_pty(")
            .expect("send_mux_pty must follow deliver_mux_pty");
        let body = &after_fn[..fn_end];

        assert!(
            body.contains("send_mux_pty(deps, &params)"),
            "deliver_mux_pty must call send_mux_pty. Body:\n{body}"
        );
        assert!(
            body.contains("strict: true,"),
            "deliver_mux_pty must pass `strict: true`. This pins QS-3 at the \
             carrier entry every `LaneOps::deliver` pane arm reaches. Body:\n{body}"
        );
        // Negative control: must NOT pass false (which is the send:pty compat value).
        assert!(
            !body.contains("strict: false,"),
            "deliver_mux_pty must NOT pass `strict: false`. Body:\n{body}"
        );
        // And `wait` is hard-coded off: a carrier can never reach the hand-back.
        assert!(
            body.contains("wait: false,"),
            "deliver_mux_pty must pass `wait: false` — PtyOutcome::Await is \
             unreachable from a carrier by construction. Body:\n{body}"
        );
    }
}
