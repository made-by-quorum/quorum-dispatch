//! The `qd start -p` PRIMING SEND — the sixth delivery body, and the only one no
//! carrier reaches.
//!
//! Nothing here prints and nothing here exits; see [`super`].
//!
//! ── WHY THIS IS HERE AND NOT BEHIND A LANE CALL ─────────────────────────────
//! `11-stage3-plan.md`'s ledger split ruled that qw's delivery records are
//! written by qw. This body was the last exception: `-p`'s send typed through
//! [`crate::submit`] inside qd's own `run_new` and emitted its own
//! `send-initiated`, `chunks-delivered`, `turn-anchored`, `turn-anchored-mismatch`
//! and (via [`WatchGuard`]) `pending-abandoned`, from a qd verb.
//!
//! It could not follow the other five through [`crate::contract::LaneOps`], and
//! the reasons are recorded rather than left to be rediscovered:
//!
//! - **Not [`LaneOps::start`](crate::contract::LaneOps::start).**
//!   [`crate::lanes::LaneImpl`]'s claude arm REFUSES `StartRequest::prompt`, and
//!   the refusal's own wording says why: the claude create returns at boot-ready
//!   and the `-p` turn is a POST-boot delivery with its own went-busy exit
//!   contract. It also runs AFTER `qd start`'s bind phase — a qd-side phase that
//!   `start` returns before — so a prompt carried into `start` would have to be
//!   delivered at a point in the choreography `start` does not reach.
//! - **Not [`LaneOps::deliver`](crate::contract::LaneOps::deliver).** It stamps
//!   `verb: "send:pty"` where the recovery sweep looks for `"new-p"`; its budget
//!   is the pane carrier's, not `DELIVER_TIMEOUT_S`; and it mints terminals on
//!   failure arms this path deliberately leaves open (see the `DeliverOutcome`
//!   discriminator below). Every one of those is a behaviour change. Its CARRIER
//!   CHOICE is a different matter, and is now this body's too — see the next
//!   section.
//!
//! So the body moved by OWNERSHIP rather than by address, the way phase 3B moved
//! the other five: a core here that answers, a `qd start` wrapper that prints and
//! chooses an exit code. What stayed in the verb is exactly what is qd's — the
//! INTENT record (ruling D11), the `map_deliver_outcome` exit contract, and the
//! stderr lines this core returns as [`Notes`] or as [`PrimingError`].
//!
//! ── THE CARRIER: RELAY FIRST, THE PANE AS FALLBACK ──────────────────────────
//! This body used to TYPE the `-p` prompt into the pane UNCONDITIONALLY, and the
//! bullet above used to cite "a session with a recorded relay port would be
//! primed over HTTP instead of typed into its pane" as a REASON against
//! `LaneOps::deliver`. That reason is RETIRED. It was never an argument: it named
//! a difference without saying why typing was better. **Typing is WORSE**, and the
//! mechanism is measured rather than suspected:
//!
//! - **The pty master blocks at 1022 bytes.** macOS `MAX_INPUT`/`MAX_CANON` is
//!   1024 and the tty input queue caps at `TTYHOG - 2`; a child-side input FLUSH
//!   during Claude Code's boot DISCARDS what is queued and keeps what is written
//!   after it. A 1048-byte `-p` prompt reached the model as its trailing 26 bytes
//!   on SIX consecutive runs, and the session then obeyed the fragment and idled
//!   for 317s. The SIZE of the loss is a kernel capacity, not a tuning knob; only
//!   WHETHER it fires is the boot race. (ADR 0009's "~4096B" is factually wrong —
//!   the number it names is not the bound that binds.)
//! - **The relay is already the PREFERRED carrier for every other claude
//!   delivery.** [`crate::lanes::claude_carrier`] answers `Relay{port}` whenever a
//!   port is recorded — relay precedence there is STRUCTURAL — and the pane is
//!   what a session with NO relay falls back to. Priming was the one claude
//!   delivery that never asked the question.
//! - **The relay is proven LIVE before `-p` runs.** Relay readiness is DEFAULT-ON
//!   for `start` (A-3, spec D5; `--no-await-relay` is the opt-out), and
//!   [`crate::boot::BootPhase::Relay`] completes BEFORE priming is reached — that
//!   phase blocks on the child's OWN sidecar appearing, matched by the sessionId
//!   off its registry row. This body resolves its port from that same sidecar, so
//!   the target it POSTs to is the one the boot gate waited for.
//! - **The relay produces a RECEIPT the pane cannot.** The child's `message-seen`
//!   — written by the relay server's received-observer when the message actually
//!   appears in the child's transcript — is an END-TO-END landed signal: measured
//!   at 200-760ms behind a ~3ms POST. The pane carrier has no equivalent; it can
//!   watch for went-busy and read a transcript back, but never for the child
//!   saying it saw the message.
//!
//! So: a session with a recorded relay port is PRIMED OVER ITS RELAY, and one
//! without is TYPED INTO ITS PANE exactly as before. The fallback is not a new
//! rule — it is `claude_carrier`'s own `(None, true) => MuxPty`, taken by calling
//! `claude_carrier` itself so the precedence here can never drift from the
//! precedence the other five deliveries obey.
//!
//! **The carrier is used DIRECTLY** ([`crate::delivery::relay::send_claude_relay`])
//! rather than by adopting `LaneOps::deliver`, and using it directly is precisely
//! what PRESERVES the three objections above: the verb stays `"new-p"` (the
//! recovery sweep's key, `verbs/intent.rs` + [`crate::events::verb_str`]), the
//! budget stays `DELIVER_TIMEOUT_S`, and NO arm of this path mints a terminal it
//! did not mint before — the receipt is READ with
//! [`crate::sendpty::watch_terminal`], which is a pure read that emits nothing.
//! The exit contract is preserved by MEANING, not by carrier: see
//! [`receipt_outcome`].

use std::cell::Cell;
use std::path::{Path, PathBuf};

use crate::boot::Sleeper;
use crate::delivery::pty::{
    ack_source_label, emit_w8_anchored, emit_w8_mismatch, send_text_chunked_mux,
};
use crate::delivery::relay::{relay_paths, send_claude_relay, RelaySendDeps};
use crate::delivery::{CarrierError, Notes, Refused, SendParams};
use crate::effects::{Clock, Env};
use crate::events::{self, EventRecord, EventWriter, Payload, WatchGuard};
use crate::lanes::{claude_carrier, ClaudeCarrier};
use crate::model::{RelayHealth, Session};
use crate::mux::Mux;
use crate::paths::QdPaths;
use crate::provider::claude::relay::RelayContract;
use crate::sendpty::TerminalWatchDeps;
use crate::submit::{
    chunk_text, deliver_prompt, payload_needs_verify, DeliverDeps, DeliverOutcome,
    PayloadVerifyOutcome, RealDeliverDeps, VerifyDeps, DELIVER_TIMEOUT_S, TWO_WRITE_SETTLE_MS,
    VERIFY_POLL_MS, VERIFY_TIMEOUT_S,
};

// ===========================================================================
// The injected effects, and the owned inputs
// ===========================================================================

/// Everything the priming send cannot own.
///
/// `paths` is the `.claude`-layout [`QdPaths`] the registry and transcript lookups
/// key on; `home` is what the QD_HOME-honouring **delivery log** root is derived
/// from. They are separate for the same reason [`super::SendDeps`] keeps them
/// separate: the two live under different roots, and collapsing them would move
/// the ledger.
pub struct PrimingDeps<'a> {
    pub env: &'a dyn Env,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
    pub mux: &'a dyn Mux,
    pub paths: &'a QdPaths,
    /// `HOME`, for the QD_HOME-honouring state dir the delivery log lives under.
    pub home: &'a Path,
    /// The CANONICAL socket dir the session was just born in — never a re-resolved
    /// `ZMX_DIR`, which may point elsewhere (ADR 0009, Bug D).
    pub socket_dir: &'a Path,
}

/// The owned inputs of one priming send.
pub struct PrimingParams<'a> {
    /// The session name. This send is addressed by NAME and not by id: at `-p`
    /// time the provider uuid may not have been written yet, which is exactly why
    /// the ledger key below falls back to `byname-<name>`.
    pub name: &'a str,
    /// The `-p` text. The caller has already established it is non-empty.
    pub prompt: &'a str,
    /// The provider session id, if the registry row carrying it had landed by the
    /// time the caller wrote its INTENT record — resolved by the caller and handed
    /// in rather than re-read here, because the key choice is STICKY for all of
    /// this send's events (§4.1) and BOTH logs' records must land under the same
    /// one. A second read in here could see a row the intent record did not, and
    /// key the two halves of one send differently.
    pub session_id: Option<&'a str>,
    /// The send id the CALLER minted, before this body ran.
    ///
    /// Ruling D10: qd's intent record has to be durable BEFORE the delivery, and
    /// it can only be written against an id that already exists. `qd start -p`
    /// mints it through `intent::record_send_intent` and hands it here, so both
    /// logs' records for this send share one id.
    pub send_id: &'a str,
}

/// The priming send reached a non-truncation end.
///
/// [`Primed::deliver`] is the three-way outcome `qd start`'s went-busy exit
/// contract maps: Accepted → 0, Stalled → 10, PidFileMissing → 1. The mapping
/// itself is the VERB's — this core neither prints nor exits.
pub struct Primed {
    /// See [`Notes`]. The two degraded-verify WARNINGs, which the pre-move body
    /// wrote inline and which are not refusals: the send stands, and the verify
    /// simply could not see it.
    pub notes: Notes,
    pub deliver: DeliverOutcome,
    /// Which carrier actually ran — `true` for the relay, `false` for the pane.
    ///
    /// Carried for the CALLER'S PROSE, not for its exit code: `DeliverOutcome`
    /// is carrier-blind on purpose (`map_deliver_outcome` maps the same three
    /// outcomes to the same three codes either way, ADR 0008 untouched). But the
    /// `Stalled` warning names a COMPOSER, and a relay injection has none — it is
    /// an HTTP POST into the child's MCP relay. Telling an operator to go look in
    /// a composer that was never typed into sends them to the wrong place, so the
    /// caller needs to know which sentence is true.
    pub relayed: bool,
}

/// Why the priming send refused.
///
/// DELIBERATELY NO `Display` — see [`CarrierError`]. The one variant's line was
/// never `qd <verb>:`-attributed, so it ignores the verb.
#[derive(Debug)]
pub enum PrimingError {
    /// POSITIVE truncation evidence from the W8 read-back. The turn STARTED, so
    /// this is a distinct named error inside the EXISTING exit-1 failure class
    /// (ADR-0008 codes untouched; ADR-0012) and NEVER an auto-retry: the truncated
    /// turn already reached the model (M11 §2).
    PayloadTruncated {
        name: String,
        expected: usize,
        recorded: usize,
    },
    /// The RELAY carrier refused — a failed POST, or no reachable relay where the
    /// carrier selection had just observed a sidecar. This is the exit-contract's
    /// THIRD arm ("transport refusal / no reachable relay when one was expected"),
    /// and it is exit 1 rather than 10 because 10 means "the session exists and
    /// the prompt is unconfirmed": here the prompt did not cross a transport at
    /// all, so there is nothing to be unconfirmed about.
    ///
    /// `carrier_line` is the relay carrier's OWN refusal sentence, rendered by
    /// [`crate::delivery::relay::RelaySendError::line`] and carried verbatim so
    /// the two verbs cannot word one failure two ways. What this variant ADDS is
    /// the fact only a CREATE verb has to state: the session was made and is
    /// running, so an exit 1 here must not read as "nothing happened".
    RelayRefused { name: String, carrier_line: String },
}

impl CarrierError for PrimingError {
    fn line(&self, _verb: &str) -> Option<String> {
        match self {
            PrimingError::PayloadTruncated {
                name,
                expected,
                recorded,
            } => Some(format!(
                "ERROR: payload truncated in delivery to \"{name}\": expected {expected} bytes, \
                 recorded {recorded}.\n  The turn started (went busy) — do NOT blindly resend \
                 (double-submit risk).\n  Attach: qd attach {name}"
            )),
            PrimingError::RelayRefused { name, carrier_line } => Some(format!(
                "{carrier_line}\n  Session \"{name}\" IS RUNNING; the -p prompt was NOT \
                 delivered over its relay.\n  Attach: qd attach {name}"
            )),
        }
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

// ===========================================================================
// The body
// ===========================================================================

/// Deliver `qd start -p`'s first turn into the freshly-booted session — over its
/// RELAY when it has a recorded one, into its PANE when it does not.
///
/// The carrier choice is [`claude_carrier`]'s, the fallback is that function's own
/// `(None, true) => MuxPty`, and the header records why the relay wins. Both arms
/// answer the SAME three-way [`DeliverOutcome`] the caller's went-busy exit
/// contract maps, so `map_deliver_outcome` needs no carrier knowledge and gets
/// none.
pub fn prime_new_session(
    deps: &PrimingDeps,
    params: &PrimingParams,
) -> Result<Primed, Refused<PrimingError>> {
    // The real HTTP relay client, built HERE and nowhere else on this path.
    //
    // It is not a [`PrimingDeps`] field, and that is a deliberate constraint
    // rather than an oversight: `PrimingDeps` is built by `qd start`'s verb, so a
    // new field is an edit to that verb. [`prime_with_relay`] is the injection
    // seam instead — same body, client passed in — which is what the tests drive.
    // `LaneOps::deliver`'s relay arm builds its client inline for the same reason.
    prime_with_relay(
        deps,
        params,
        &crate::provider::claude::relay::http::CcRelay::new(),
    )
}

/// [`prime_new_session`] with the relay transport INJECTED — the seam, and the
/// one both carrier arms run through.
///
/// The writer is built here, ONCE, because the ledger key is a property of the
/// send and not of the carrier: sessionId when the caller resolved one, else
/// `byname-<name>`, sticky for every event of this send (§4.1) across BOTH logs.
/// A carrier that re-derived it could file the two halves of one send under two
/// names.
pub(crate) fn prime_with_relay(
    deps: &PrimingDeps,
    params: &PrimingParams,
    relay: &dyn RelayContract,
) -> Result<Primed, Refused<PrimingError>> {
    // --- ACK-2 §9 (M3): engine event emission for the -p send (best-effort) ---
    // events key: sessionId if resolvable NOW (the existing non-blocking registry
    // read), else byname(name) — the key choice is STICKY for ALL of this send's
    // events (§4.1). state_dir honors QD_HOME (§4.1 / ADD-14).
    let ev_state = QdPaths::from_home_env(deps.home, deps.env).state_dir;
    let ev_session_id = params.session_id.map(str::to_string);
    let ev_key = ev_session_id
        .clone()
        .unwrap_or_else(|| events::byname_key(params.name));
    let writer = EventWriter::for_key(
        &ev_state,
        &ev_key,
        ev_session_id.clone(),
        Some(params.name.to_string()),
    );

    // The carrier, decided by the SAME function `LaneOps::deliver` decides with.
    //
    // `joined_pane: true` is not an assumption — it is a PRECONDITION the caller
    // has already enforced: `qd start -p` refuses to call this body at all when
    // the create reported no socket dir ("typing the prompt into the wrong pane is
    // worse than not typing it"), and a pane create that reached boot-ready has
    // its `zmx_name`. So `NoLiveReceivePath` is structurally unreachable here, and
    // the arm that matters is the one it shares with `MuxPty`: no relay ⇒ type it,
    // exactly as this body always did.
    let target = relay_target(deps, params);
    let carrier = claude_carrier(target.as_ref().map(|t| t.port), true);
    match (carrier, target) {
        (ClaudeCarrier::Relay { port }, Some(t)) => {
            prime_over_relay(deps, params, &writer, &ev_state, relay, port, &t.session)
        }
        // MuxPty, the unreachable NoLiveReceivePath, and the structurally
        // impossible Relay-with-no-target all land on the pane: the fallback is
        // never a refusal, because a session with no relay is a session this body
        // has always been able to prime.
        _ => prime_over_pty(deps, params, &writer),
    }
}

/// Type `qd start -p`'s first turn into the freshly-booted session's pane.
///
/// ── WHAT THIS EMITS, AND WHAT IT DELIBERATELY DOES NOT ──────────────────────
/// `send-initiated` before the first chunk write, `chunks-delivered` when EVERY
/// text chunk acked, and — on POSITIVE verify evidence only — `turn-anchored` or
/// `turn-anchored-mismatch`. The [`WatchGuard`] is armed across the
/// deliver-acceptance watch and the verify so an unwind cannot leave the watch
/// without a terminal.
///
/// **No arm of the deliver outcome mints a terminal**, and that is ruling D8's
/// shape rather than an omission — see the discriminator at the end of the body.
fn prime_over_pty(
    deps: &PrimingDeps,
    params: &PrimingParams,
    writer: &EventWriter,
) -> Result<Primed, Refused<PrimingError>> {
    let name = params.name;
    let p = params.prompt;
    let send_id = params.send_id;
    let mut notes = Notes::new();

    // D10 (R6/R7): the transcript+offset snapshot is UNCONDITIONAL-when-resolvable
    // (not only when payload_needs_verify). The verify step still uses the SAME
    // offset; a single-chunk send simply doesn't run verify.
    let ev_transcript = resolve_new_p_transcript(deps.paths, name);
    let snapshot_offset: u64 = ev_transcript
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok().map(|m| m.len()))
        .unwrap_or(0);
    // The verify window offset = the snapshot when verify runs, else unused.
    let verify_offset: u64 = if payload_verify_applies(ClaudeCarrier::MuxPty, p) {
        snapshot_offset
    } else {
        0
    };

    // §2.3.1 send-initiated (verb:"new-p", send_path:"idle"): minted BEFORE the
    // first chunk write. Per-chunk + content shas from the production splitter;
    // transcript/offset present when resolvable (D10).
    let chunks_vec = chunk_text(p, events::CHUNK_BYTES);
    let chunk_sha256s: Vec<String> = chunks_vec
        .iter()
        .map(|c| events::sha256_hex(c.as_bytes()))
        .collect();
    let ev_transcript_str = ev_transcript.as_ref().map(|p| p.display().to_string());
    let ev_transcript_offset = ev_transcript.as_ref().map(|_| snapshot_offset);
    events::warn_emit(
        writer,
        deps.clock,
        &Payload::SendInitiated {
            send_id: send_id.to_string(),
            verb: events::verb_str(true).to_string(),
            send_path: "idle".to_string(),
            content_sha256: events::sha256_hex(p.as_bytes()),
            content_len: p.len() as u64,
            chunks: chunks_vec.len() as u32,
            chunk_sha256s,
            chunk_sha256s_capped: false,
            transcript: ev_transcript_str,
            transcript_offset: ev_transcript_offset,
            // ADD-20 (§6.2): redacted ≤256B preview of the -p prompt text.
            content_preview: Some(quorum_core::redact::redact_for_preview(
                p,
                events::PREVIEW_CAP_BYTES,
            )),
        },
    );

    // §9: the deliver runs through a RECORDING DeliverDeps (Real* binding +
    // per-chunk ack capture; the DeliverDeps trait + pure deliver_prompt core are
    // UNTOUCHED). chunks-delivered emits when EVERY text chunk acked.
    let deliver = RecordingDeliverDeps {
        inner: RealDeliverDeps {
            mux: deps.mux,
            clock: deps.clock,
            sleeper: deps.sleeper,
            zmx_name: name.to_string(),
            session_name: name.to_string(),
            sessions_dir: deps.paths.sessions_dir.clone(),
            dir: deps.socket_dir.to_path_buf(),
        },
        mux: deps.mux,
        dir: deps.socket_dir.to_path_buf(),
        zmx_name: name.to_string(),
        sleeper: deps.sleeper,
        total: Cell::new(0),
        acked: Cell::new(0),
    };

    // rev C row 24 WatchGuard: armed across the deliver-acceptance watch + verify;
    // an early return / panic without a terminal Drops it →
    // pending-abandoned{watch-interrupted}. Disarmed on every terminal below.
    let clock_ref = ClockRef(deps.clock);
    let guard = WatchGuard::arm(writer, &clock_ref, send_id);

    let outcome = deliver_prompt(&deliver, p, DELIVER_TIMEOUT_S);

    // §2.3.2 chunks-delivered: all text chunks acked → emit.
    let acks_total = deliver.total.get();
    let acks_acked = deliver.acked.get();
    if acks_total > 0 && acks_acked == acks_total {
        events::warn_emit(
            writer,
            deps.clock,
            &Payload::ChunksDelivered {
                send_id: send_id.to_string(),
                chunks_acked: acks_acked,
                // -p delivery is the embedded/zmx backend per the create lane;
                // name the channel honestly via the shared label — the SAME
                // decider `send:pty`'s carrier uses, so the two verbs can never
                // disagree about which channel acked.
                ack_source: ack_source_label(deps.env).to_string(),
            },
        );
    }

    // W8 verify-after-submit: CHUNKED deliveries that went busy get a bounded
    // payload read-back (M11 sanctioned; ADR-0012). Loud exit-1 ONLY on POSITIVE
    // truncation evidence; resolution failure / no record / foreign records
    // DEGRADE to one warn (this path's transcript may simply not be resolvable yet
    // — false-fails are the design enemy, red-team R1). Single-chunk prompts:
    // behavior byte-for-byte unchanged (scope guard).
    if outcome == DeliverOutcome::Accepted && payload_verify_applies(ClaudeCarrier::MuxPty, p) {
        let verify_deps = NewPVerifyDeps {
            paths: deps.paths,
            name,
            offset: verify_offset,
            clock: deps.clock,
            sleeper: deps.sleeper,
        };
        match crate::submit::verify_chunked_payload(
            &verify_deps,
            p,
            VERIFY_TIMEOUT_S,
            VERIFY_POLL_MS,
        ) {
            PayloadVerifyOutcome::Verified => {
                // §9 anchored: a Verified read-back IS the landed signal. recovered
                // false; attribution absent; the anchor uses the verify-window
                // offset, line_index 0 (verify returns texts, not indices —
                // documented unknown). An unresolved transcript is the empty path,
                // which is the empty string the pre-move emitter wrote.
                emit_w8_anchored(
                    writer,
                    deps.clock,
                    send_id,
                    p,
                    ev_transcript.as_deref().unwrap_or(Path::new("")),
                    verify_offset,
                    0,
                );
                guard.disarm();
                return Ok(Primed {
                    notes,
                    deliver: outcome,
                    relayed: false,
                });
            }
            PayloadVerifyOutcome::Truncated { expected, recorded } => {
                // §2.3.4 anchored-mismatch (terminal): the lengths come from the
                // outcome; actual_sha is re-derived by re-reading past the offset +
                // sha-ing the longest truncation signature. Emitted BEFORE the
                // unchanged exit-1 the caller renders.
                emit_w8_mismatch(
                    writer,
                    deps.clock,
                    send_id,
                    p,
                    ev_transcript.as_deref().unwrap_or(Path::new("")),
                    verify_offset,
                    expected,
                    recorded,
                );
                guard.disarm();
                return Err(Refused {
                    notes,
                    error: PrimingError::PayloadTruncated {
                        name: name.to_string(),
                        expected,
                        recorded,
                    },
                    message_id: Some(send_id.to_string()),
                });
            }
            PayloadVerifyOutcome::Unattributable => {
                // No terminal (spec §9: NoRecord/Unattributable/SourceUnavailable →
                // stays dangling). The send remains outstanding by design.
                notes.push(format!(
                    "WARNING: could not attribute the delivered payload in \"{name}\"'s \
                     transcript — check: qd attach {name}"
                ));
            }
            PayloadVerifyOutcome::NoRecord | PayloadVerifyOutcome::SourceUnavailable(_) => {
                notes.push(format!(
                    "WARNING: could not verify payload delivery to \"{name}\" \
                     (transcript not yet resolvable) — check: qd attach {name}"
                ));
            }
        }
    }

    // §9 / §C2 (R5 seam ruling 01KX88WKGP + amend rider 3, red-team finding G):
    // the deliver outcome mints NO foreclosing terminal on ANY arm. All three
    // outcomes fire in the SAME post-deliver-attempt match, and deliver_prompt
    // writes the message to the pty (send_message: chunks + `\r`, submit.rs:579 /
    // RecordingDeliverDeps::send_message) BEFORE it can reach ANY of these
    // outcomes — so each is a post-wire, possibly-LANDED priming send whose fate is
    // in-band-undeterminable here:
    //   - Accepted (single-chunk, or chunked-with-degraded-verify): NO terminal —
    //     written+accepted; the anchor comes from verify only (the two verify arms
    //     above already emitted turn-anchored / -mismatch on POSITIVE observation).
    //   - Stalled: the deliver budget (DELIVER_TIMEOUT_S) expired while watching for
    //     turn-start — the bytes were written + `\r` submitted, so the turn may yet
    //     commit. An `anchor-timeout` here would FALSE-FAIL a possibly-landed prime
    //     and FORECLOSE recovery (same class as the send:pty TimedOut arm).
    //   - PidFileMissing: `find_pid_file` returned None AFTER send_message already
    //     wrote the chunks + `\r` (deliver_prompt: send_message at submit.rs:579
    //     precedes the None return at :583-584) — the registry row vanished
    //     post-write, so the bytes may have landed before the session died. A
    //     `pending-abandoned{session-died}` here would FALSE-FAIL a possibly-landed
    //     prime and FORECLOSE recovery (same class as the send:pty Died arm). This
    //     CORRECTS the door-inventory's "priming send already covered" — only the
    //     Accepted arm was non-foreclosing; its failure arms foreclosed.
    // So NO terminal on any arm: the priming send stays dead-dangling once the
    // caller exits, and `qd delivery:recover` (its sweep includes verb "new-p")
    // closes it from the transcript — turn-anchored{recovered} if it landed, else
    // pending-abandoned{recovery-no-candidate}. The LOUD operator signal + exit
    // codes (Stalled → 10 WARNING; PidFileMissing → 1 ERROR) are the CALLER's
    // `map_deliver_outcome` and are UNCHANGED — the C1 account is the standing
    // send-initiated + that loud synchronous exit + C2's PENDING-closable state.
    // The exhaustive match is kept so any future DeliverOutcome variant is forced
    // back through this same discriminator (F3's coverage-hole lesson).
    match outcome {
        DeliverOutcome::Stalled => {}
        DeliverOutcome::PidFileMissing => {}
        DeliverOutcome::Accepted => {}
    }
    guard.disarm();
    Ok(Primed {
        notes,
        deliver: outcome,
        relayed: false,
    })
}

// ===========================================================================
// The relay arm
// ===========================================================================

/// A relay this priming send can actually reach: the port, and the row the
/// carrier addresses by.
struct RelayTarget {
    port: u16,
    session: Session,
}

/// Resolve the recorded relay for this brand-new session, or `None`.
///
/// **The sidecar, not the process table.** [`crate::lanes::LaneImpl::relay_port_for`]
/// discovers a port by walking process ANCESTRY, because a `qd send` may address a
/// session whose relay was never seen born. Priming's session was born seconds
/// ago behind [`crate::boot::BootPhase::Relay`], which blocks until a sidecar
/// carrying THIS session's id exists — so the same sidecar read that gate used
/// answers here exactly, with no `ps`, no HTTP port-scan fallback, and no chance
/// of matching a neighbour's relay: the join key is the session uuid.
///
/// `None` — no session id yet, no matching sidecar, or no registry row to address
/// by — is a plain "no recorded relay", which [`claude_carrier`] turns into the
/// pane. It is never an error, because a claude session without a relay is a
/// session this body has always primed by typing.
fn relay_target(deps: &PrimingDeps, params: &PrimingParams) -> Option<RelayTarget> {
    let sid = params.session_id.filter(|s| !s.is_empty())?;
    let sidecars = crate::relay::read_sidecars(&deps.paths.relay_dir);
    let port = sidecar_relay_port(&sidecars, Some(sid))?;
    // The row the carrier injects against. `row_for_id` is qw's exact, id-keyed
    // lookup — the same one every rewired verb uses — so the `SessionKey.id`
    // `send_claude_relay` builds is the RESOLVED uuid (QS-2), never a display
    // name. A row that has not landed yet degrades to the pane rather than being
    // faked: a synthesised `Session` would be this body asserting a row it never
    // read.
    let session = crate::lanes::row_for_id(
        deps.paths,
        deps.env,
        None,
        &crate::contract::SessionId(sid.to_string()),
    )?;
    Some(RelayTarget { port, session })
}

/// The sidecar → port join, PURE. `session_id` is the child's provider uuid; a
/// sidecar counts only when it names that exact id (the match
/// [`crate::boot`]'s relay phase waits on).
pub(crate) fn sidecar_relay_port(
    sidecars: &[RelayHealth],
    session_id: Option<&str>,
) -> Option<u16> {
    let sid = session_id.filter(|s| !s.is_empty())?;
    sidecars
        .iter()
        .find(|h| h.session_id == sid)
        .map(|h| h.port)
}

/// Inject `qd start -p`'s first turn over the freshly-booted session's RELAY.
///
/// ── WHAT THIS EMITS ─────────────────────────────────────────────────────────
/// `send-initiated` with verb `"new-p"` and the CALLER's send id — the priming
/// send's identity is a property of the send, not of the carrier, and `qd
/// delivery:recover`'s sweep plus qd's intent record (ruling D10/D11) both key on
/// it. `send_path` is `"relay"` because that field names an OBSERVATION and the
/// observation changed; `chunks: 1` because the payload crosses as ONE body.
///
/// Then [`send_claude_relay`] — the SAME carrier `LaneOps::deliver` uses, called
/// directly — which emits its own transport pair (`send-initiated{send:relay}` +
/// `relay-delivered`) keyed on the RELAY-minted message id. Those two ids are
/// deliberately different and neither is invented here: the relay's id is the one
/// the child's `message-seen` will join on, and ours is the one the intent record
/// already carries.
///
/// ── AND WHAT IT DELIBERATELY DOES NOT ───────────────────────────────────────
/// **No terminal, on any arm.** The receipt is READ, with
/// [`crate::sendpty::watch_terminal`] — a pure poll of the ledger that emits
/// nothing, chosen over `events::await_received` for exactly the reason the
/// embedded `--wait` path chose it: that helper is a WRITER (it mints
/// `anchor-timeout`), and minting a foreclosing terminal on a possibly-landed
/// prime is what this module's discriminator exists to prevent. The relay
/// server's observer owns the terminal for the message it minted the id for.
///
/// **No W8 verify** — see [`payload_verify_applies`].
///
/// **No stdout.** [`Delivered::stdout`](crate::delivery::Delivered::stdout) is the
/// relay message id, which `qd send:relay` echoes and `qd start -p` never has:
/// this verb's stdout is the session, and its receipt line is
/// `map_deliver_outcome`'s. The id is dropped here rather than returned, because
/// [`Primed`] carrying it would put it one field away from being printed.
#[allow(clippy::too_many_arguments)]
fn prime_over_relay(
    deps: &PrimingDeps,
    params: &PrimingParams,
    writer: &EventWriter,
    ev_state: &Path,
    relay: &dyn RelayContract,
    port: u16,
    session: &Session,
) -> Result<Primed, Refused<PrimingError>> {
    let name = params.name;
    let p = params.prompt;
    let send_id = params.send_id;
    let mut notes = Notes::new();

    // §2.3.1 send-initiated — verb "new-p" (UNCHANGED: the recovery sweep's key),
    // send_path "relay", ONE chunk. NO transcript/transcript_offset recovery keys:
    // those are the pty path's anchor for a byte-exact transcript read-back, and a
    // relay message lands in the transcript inside a `<channel …>` envelope, so an
    // offset carried here would advertise an anchor no exact-match search can hit.
    // The daemon carriers omit them for the same reason (`super::emit_daemon_send_events`).
    let content_sha256 = events::sha256_hex(p.as_bytes());
    events::warn_emit(
        writer,
        deps.clock,
        &Payload::SendInitiated {
            send_id: send_id.to_string(),
            verb: events::verb_str(true).to_string(),
            send_path: "relay".to_string(),
            content_sha256: content_sha256.clone(),
            content_len: p.len() as u64,
            chunks: 1,
            chunk_sha256s: vec![content_sha256],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            // ADD-20 (§6.2): redacted ≤256B preview of the -p prompt text.
            content_preview: Some(quorum_core::redact::redact_for_preview(
                p,
                events::PREVIEW_CAP_BYTES,
            )),
        },
    );

    // The budget is PRIMING's — DELIVER_TIMEOUT_S, not the relay carrier's default
    // — and it is taken from BEFORE the POST so this arm can never outlast the
    // pane arm's own deliver budget. The POST is measured in single-digit ms; what
    // spends the budget is the wait for the child's receipt.
    let started_ms = deps.clock.now_ms();

    let carrier_paths = relay_paths(deps.env);
    let relay_deps = RelaySendDeps {
        env: deps.env,
        paths: &carrier_paths,
        clock: deps.clock,
        relay,
        relay_port: port,
    };
    let delivered = match send_claude_relay(
        &relay_deps,
        &SendParams {
            session,
            message: p,
            send_id,
        },
    ) {
        Ok(d) => d,
        Err(refused) => {
            // Exit-contract arm 3: a transport refusal where a relay WAS expected.
            // The carrier's notes come first (its own, in emission order), then its
            // refusal sentence carried verbatim inside ours.
            notes.extend(refused.notes);
            return Err(Refused {
                notes,
                error: PrimingError::RelayRefused {
                    name: name.to_string(),
                    carrier_line: refused
                        .error
                        .line("start")
                        .unwrap_or_else(|| format!("Session \"{name}\" has no relay.")),
                },
                // Keyed: the `send-initiated` above is already durable under this
                // id, so the refusal carries it (`CarrierOutcome::keyed`).
                message_id: Some(send_id.to_string()),
            });
        }
    };
    notes.extend(delivered.notes);

    // The RECEIPT: the child's own `message-seen`, keyed on the relay-minted id,
    // written into THIS session's ledger by the relay server's received-observer
    // when the message appears in the transcript. Joined by id only — the relay
    // server's own docs call the extracted body "advisory … never a join key"
    // (§X.4), so gating a landed prime on byte-equality with a channel-wrapped
    // body would false-fail on a wrapper artifact.
    let watch = ReceiptWatch {
        state_dir: ev_state,
        session_id: params.session_id,
        name,
        clock: deps.clock,
        sleeper: deps.sleeper,
    };
    let terminal = crate::sendpty::watch_terminal(
        &watch,
        &delivered.message_id,
        remaining_budget_ms(started_ms, deps.clock.now_ms()),
        // The same 500ms cadence every other ledger/transcript poll in this crate
        // uses (`--wait`, the W8 verify). The receipt lands 200-760ms behind the
        // POST, so one or two polls is the normal cost.
        VERIFY_POLL_MS,
    );

    Ok(Primed {
        notes,
        deliver: receipt_outcome(terminal.as_ref()),
        relayed: true,
    })
}

/// THE EXIT CONTRACT ON THE RELAY PATH (ADR 0008, `doc/PROTOCOL.md`).
///
/// `map_deliver_outcome` is went-busy-keyed and stays that way; what changes is
/// only the EVIDENCE the three meanings are read off. The receipt is a strictly
/// better witness than went-busy — it is the child reporting the message in its
/// own transcript, not the pane reporting a status flip — so it is mapped onto the
/// same three answers rather than onto new ones:
///
/// - a SUCCESS terminal (`message-seen`) for our relay id inside the budget → the
///   prompt was accepted → [`DeliverOutcome::Accepted`] → **0**.
/// - nothing inside the budget → delivered, unconfirmed →
///   [`DeliverOutcome::Stalled`] → **10**. This is exactly what 10 is for: the
///   session exists and is addressable, and we decline to claim its first turn
///   started. Honest-pending, never a false failure.
/// - a NON-success terminal for our id (the ledger already closed this message as
///   not-delivered) → also **10**, not 1. Exit 1 on this path means the transport
///   refused ([`PrimingError::RelayRefused`]); a message that crossed the wire and
///   was then closed negatively is an unconfirmed prompt, not a broken transport,
///   and reporting it as infra failure would misroute the operator.
///
/// [`DeliverOutcome::PidFileMissing`] is unreachable here and that is not a gap:
/// it names the pty path's post-write discovery that the registry row vanished,
/// an observation the relay carrier never makes.
pub(crate) fn receipt_outcome(terminal: Option<&EventRecord>) -> DeliverOutcome {
    match terminal {
        Some(rec) if events::is_success_terminal(&rec.event) => DeliverOutcome::Accepted,
        _ => DeliverOutcome::Stalled,
    }
}

/// What is LEFT of the priming budget, in ms, never negative.
///
/// Constraint 2 spelled as arithmetic: the relay arm's total wall time is bounded
/// by [`DELIVER_TIMEOUT_S`] — priming's own budget — and the POST is inside it,
/// not before it.
pub(crate) fn remaining_budget_ms(started_ms: i64, now_ms: i64) -> i64 {
    (DELIVER_TIMEOUT_S as i64 * 1000 - (now_ms - started_ms)).max(0)
}

/// Whether W8's chunked-payload verify applies to this carrier's delivery.
///
/// ── THE RULING, AND WHY IT IS NOT AMBIGUOUS ─────────────────────────────────
/// W8's read-back exists for ONE failure mode: a CHUNKED tty write can silently
/// lose bytes. `payload_needs_verify` is therefore a question about the pty — is
/// this payload big enough to be split — and on the relay path all three of its
/// premises are gone:
///
/// 1. **Nothing is chunked.** The payload crosses as a single `POST /message`
///    body. There is no splitter, no inter-chunk settle, and no line discipline
///    to saturate — the 1022-byte kernel bound this carrier exists to escape
///    cannot apply to an HTTP body.
/// 2. **A partial body is a REFUSAL, not a silent short write.** A body that does
///    not complete is a [`crate::provider::claude::relay::RelayError`], which is
///    already exit 1 through [`PrimingError::RelayRefused`]. There is no
///    "delivered but shorter" state for the verify to catch.
/// 3. **Running it anyway would MANUFACTURE false warnings.** A relay message
///    lands in the transcript wrapped in a `<channel …>` envelope, so
///    `verify_chunked_payload`'s exact / chunk-prefix match against the raw
///    payload cannot attribute it — every relay prime over 1024B would degrade to
///    `Unattributable` and print a WARNING about a delivery that demonstrably
///    succeeded. False-fails are the design enemy (red-team R1).
///
/// And the thing it would have bought is already bought better: the receipt is an
/// end-to-end landed signal from the child itself, which is strictly more than a
/// read-back proves.
///
/// So: verify on the pane, never on the relay. The pane path's behaviour is
/// byte-for-byte unchanged, which is what routing BOTH arms through one decider
/// keeps honest.
pub(crate) fn payload_verify_applies(carrier: ClaudeCarrier, prompt: &str) -> bool {
    match carrier {
        ClaudeCarrier::Relay { .. } => false,
        ClaudeCarrier::MuxPty | ClaudeCarrier::NoLiveReceivePath => payload_needs_verify(prompt),
    }
}

/// [`TerminalWatchDeps`] over this session's own delivery ledger: a READ-ONLY
/// merged read of the (sessionId, byname) pair, the same merge every consumer
/// does. No boundary is crossed — this is qw reading the log qw wrote.
struct ReceiptWatch<'a> {
    state_dir: &'a Path,
    session_id: Option<&'a str>,
    name: &'a str,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl TerminalWatchDeps for ReceiptWatch<'_> {
    fn read_records(&self) -> Vec<EventRecord> {
        events::read_merged(self.state_dir, self.session_id, Some(self.name)).records
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// §5.1 / D6: emit `priming-readiness-timeout` to the BYNAME file on a `-p` boot
/// timeout (no sessionId exists on a failed boot). `phase` is the TYPED boot phase
/// carried from the source on `create::NewError::BootTimeout` (m-4, ack3-spec §8):
/// `Idle` → "idle", `PidFile` → "pid-file" — no longer string-matched out of the
/// detail wording. `waited_ms` is best-effort (the configured phase deadline).
///
/// The caller's stderr/exit are UNCHANGED by this — it is purely additive, and it
/// is the ONE record of this send that can exist: a boot that timed out never gave
/// the session an id, so the key is `byname-<name>` and nothing else can reach it.
pub fn emit_priming_timeout(
    env: &dyn Env,
    clock: &dyn Clock,
    home: &Path,
    name: &str,
    phase: crate::boot::BootPhase,
) {
    let ev_state = QdPaths::from_home_env(home, env).state_dir;
    let writer = EventWriter::for_key(
        &ev_state,
        &events::byname_key(name),
        None,
        Some(name.to_string()),
    );
    let defaults = crate::boot::BootTimeouts::default();
    // m-4 (ack3-spec §8): phase is read TYPED from the BootFailure carried up the
    // create seam — the old `detail.contains("did not reach idle")` string-match
    // (the named COUPLING) is gone; a reworded boot error can no longer misfile it.
    let (phase, waited_ms) = match phase {
        crate::boot::BootPhase::Idle => ("idle", defaults.overall_ms.max(0) as u64),
        crate::boot::BootPhase::PidFile => ("pid-file", defaults.pid_phase_ms.max(0) as u64),
        // Fix-A (RESPEC-DELTA §4): the relay-sidecar phase shares the overall
        // deadline (it runs after idle, bounded by the same boot deadline).
        crate::boot::BootPhase::Relay => ("relay", defaults.overall_ms.max(0) as u64),
    };
    events::warn_emit(
        &writer,
        clock,
        &Payload::PrimingReadinessTimeout {
            waited_ms,
            phase: phase.to_string(),
        },
    );
}

// ===========================================================================
// The pieces the body is assembled from
// ===========================================================================

/// [`WatchGuard`] is generic over a SIZED [`Clock`]; this body holds `&dyn Clock`.
/// The same shim [`crate::delivery::pty`] uses, for the same reason.
struct ClockRef<'a>(&'a dyn Clock);

impl Clock for ClockRef<'_> {
    fn now_ms(&self) -> i64 {
        self.0.now_ms()
    }
}

/// W8: resolve the just-created session's transcript path (best-effort, single
/// pass): registry row by NAME → sessionId → `find_jsonl_path`. `None` at any step
/// (claude hasn't written its row / id / transcript yet — normal for a fresh
/// session).
fn resolve_new_p_transcript(paths: &QdPaths, name: &str) -> Option<PathBuf> {
    let entry = crate::registry::read_entries(&paths.sessions_dir, false)
        .into_iter()
        .find(|s| s.entry.name.as_deref() == Some(name))?;
    let sid = entry.entry.session_id?;
    crate::jsonl::find_jsonl_path(&paths.projects_dir, &sid, entry.entry.cwd.as_deref())
}

/// A [`DeliverDeps`] that wraps [`RealDeliverDeps`] and RECORDS each
/// `send_message` text-chunk ack (ACK-2 §9 recorder; red-team R9). `send_message`
/// re-implements the Real impl's CHUNKED two-write delivery but captures each
/// chunk's `mux.send(...).is_ok()`; the CR write is NOT counted (only text chunks
/// are chunks-delivered evidence). All OTHER trait methods delegate to `inner`, so
/// the bounded-retry core + the DeliverDeps trait are UNTOUCHED.
struct RecordingDeliverDeps<'a> {
    inner: RealDeliverDeps<'a>,
    mux: &'a dyn Mux,
    dir: PathBuf,
    zmx_name: String,
    sleeper: &'a dyn Sleeper,
    total: Cell<u32>,
    acked: Cell<u32>,
}

impl DeliverDeps for RecordingDeliverDeps<'_> {
    fn send_message(&self, message: &str) {
        // CHUNKED two-write delivery (mirrors RealDeliverDeps::send_message,
        // submit.rs), recording each text chunk's ack through the SAME shared
        // splitter the pane carrier records with. The settle + separate "\r" are
        // byte-identical to the Real impl; only the per-chunk result, which the
        // Real impl discards via `let _ =`, is captured here.
        let acks =
            send_text_chunked_mux(self.mux, self.sleeper, &self.dir, &self.zmx_name, message);
        self.total.set(self.total.get() + acks.total);
        self.acked.set(self.acked.get() + acks.acked);
        self.sleeper.sleep_ms(TWO_WRITE_SETTLE_MS);
        let _ = self.mux.send(&self.dir, &self.zmx_name, "\r");
    }
    fn read_screen(&self) -> String {
        self.inner.read_screen()
    }
    fn find_pid_file(&self) -> Option<PathBuf> {
        self.inner.find_pid_file()
    }
    fn submit_deps(
        &self,
        pid_file: PathBuf,
        message: &str,
    ) -> Box<dyn crate::submit::SubmitDeps + '_> {
        self.inner.submit_deps(pid_file, message)
    }
}

/// W8 [`VerifyDeps`] for the `qd start -p` path: RE-resolves the transcript each
/// poll (registry → sessionId → path; the fresh session's row/transcript may land
/// mid-budget) and reads user texts past the pre-delivery offset. Every resolution
/// failure is a re-polled `Err` — [`crate::submit::verify_chunked_payload`]
/// degrades to `SourceUnavailable` only when NO read ever succeeds.
struct NewPVerifyDeps<'a> {
    paths: &'a QdPaths,
    name: &'a str,
    offset: u64,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl VerifyDeps for NewPVerifyDeps<'_> {
    fn read_user_texts(&self) -> Result<Vec<String>, String> {
        let path = resolve_new_p_transcript(self.paths, self.name)
            .ok_or_else(|| "session transcript not yet resolvable".to_string())?;
        crate::submit::read_user_texts_past_offset(&path, self.offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

#[cfg(test)]
mod relay_carrier_tests {
    //! The RELAY-carrier rows: which carrier a `-p` send takes, what the receipt
    //! maps to on the caller's went-busy exit contract, and the W8-verify ruling.
    //!
    //! The exit CODES themselves are asserted where they are produced —
    //! `bin/qd/verbs/lifecycle.rs`'s `map_deliver_outcome` rows (Accepted → 0,
    //! Stalled → 10, PidFileMissing → 1). These rows pin the OTHER half: which
    //! `DeliverOutcome` each relay observation reduces to, which is what decides
    //! the code this path exits with.

    use super::*;
    use crate::effects::{FixedClock, MapEnv};
    use crate::events::parse_events;
    use crate::model::RelayHealth;
    use crate::mux::FixtureMux;
    use crate::provider::claude::relay::{RelayError, RelayReply};
    use std::cell::Cell;
    use std::collections::HashMap;

    // --- carrier selection -------------------------------------------------

    /// A recorded port takes the RELAY, and it takes it BEFORE the pane is
    /// considered — the structural precedence `claude_carrier` encodes, which this
    /// body now shares with the other five deliveries instead of ignoring.
    ///
    /// MUTATION EVIDENCE: making `relay_target` answer `None` unconditionally (or
    /// swapping the arms in `prime_with_relay`'s match) REDs this row.
    #[test]
    fn a_recorded_relay_port_selects_the_relay_carrier() {
        assert_eq!(
            claude_carrier(Some(4711), true),
            ClaudeCarrier::Relay { port: 4711 },
            "a recorded port is the relay, pane or no pane"
        );
    }

    /// No recorded port + a joined pane ⇒ TYPE IT, exactly as this body always
    /// did. The fallback is `claude_carrier`'s own `(None, true) => MuxPty`, which
    /// is why it is taken from that function rather than restated here.
    ///
    /// MUTATION EVIDENCE: routing the `MuxPty` arm to the relay body REDs this
    /// row's sibling e2e (`no_sidecar_falls_back_to_the_pane_body`); changing the
    /// decider REDs this one.
    #[test]
    fn no_relay_port_falls_back_to_the_pane() {
        assert_eq!(claude_carrier(None, true), ClaudeCarrier::MuxPty);
    }

    /// The sidecar join is on the session UUID — the SAME match
    /// `boot::run_relay_phase` blocks on — so a neighbour session's relay can
    /// never be primed into.
    ///
    /// MUTATION EVIDENCE: relaxing the match to "first sidecar" REDs the
    /// neighbour assert; dropping the empty/None guard REDs the last two.
    #[test]
    fn sidecar_port_joins_on_the_session_uuid() {
        let sidecars = vec![
            RelayHealth {
                session_id: "neighbour".to_string(),
                port: 1111,
                pid: 10,
                status: "ok".to_string(),
            },
            RelayHealth {
                session_id: "sid-1".to_string(),
                port: 4711,
                pid: 20,
                status: "ok".to_string(),
            },
        ];
        assert_eq!(sidecar_relay_port(&sidecars, Some("sid-1")), Some(4711));
        assert_eq!(sidecar_relay_port(&sidecars, Some("unknown")), None);
        assert_eq!(sidecar_relay_port(&sidecars, None), None);
        assert_eq!(sidecar_relay_port(&sidecars, Some("")), None);
        assert_eq!(sidecar_relay_port(&[], Some("sid-1")), None);
    }

    // --- the exit contract -------------------------------------------------

    /// ARM 1 — delivered AND the child's `message-seen` receipt observed inside
    /// the budget ⇒ `Accepted` ⇒ the caller exits **0**.
    ///
    /// MUTATION EVIDENCE: returning `Stalled` from the success arm of
    /// `receipt_outcome` REDs this row.
    #[test]
    fn receipt_inside_the_budget_is_accepted() {
        let rec = record(r#"{"event":"message-seen","sendId":"mid-1","pid":1,"seq":1,"ts":"t"}"#);
        assert_eq!(receipt_outcome(Some(&rec)), DeliverOutcome::Accepted);
    }

    /// ARM 2 — delivered, no receipt inside the budget ⇒ `Stalled` ⇒ **10**. The
    /// session exists and is addressable; we decline to claim its first turn
    /// started. Honest-pending, never a false failure.
    ///
    /// MUTATION EVIDENCE: answering `Accepted` on `None` REDs this row (and would
    /// turn every unconfirmed prime into a claimed one).
    #[test]
    fn no_receipt_inside_the_budget_is_stalled_not_accepted() {
        assert_eq!(receipt_outcome(None), DeliverOutcome::Stalled);
    }

    /// ARM 2, the other way in — a NON-success terminal for our id is still
    /// **10**, not 1. It crossed the wire; a negatively-closed message is an
    /// unconfirmed prompt, not a broken transport. Exit 1 on this path is reserved
    /// for a transport refusal.
    ///
    /// MUTATION EVIDENCE: dropping the `is_success_terminal` guard (accepting any
    /// terminal) REDs this row.
    #[test]
    fn a_failure_terminal_is_stalled_not_a_transport_failure() {
        let rec = record(
            r#"{"event":"pending-abandoned","sendId":"mid-1","reason":"session-died","pid":1,"seq":1,"ts":"t"}"#,
        );
        assert_eq!(receipt_outcome(Some(&rec)), DeliverOutcome::Stalled);
    }

    /// ARM 3 — a transport refusal where a relay WAS expected is exit **1**, and
    /// its line carries BOTH the carrier's own refusal sentence and the fact only
    /// a create verb has to state: the session is running.
    ///
    /// MUTATION EVIDENCE: changing `exit_code` to 10 REDs the first assert;
    /// dropping the carrier line from the format REDs the second.
    #[test]
    fn a_transport_refusal_is_exit_one_and_says_the_session_is_running() {
        let err = PrimingError::RelayRefused {
            name: "wk".to_string(),
            carrier_line: "Failed to send message: connection failed".to_string(),
        };
        assert_eq!(err.exit_code(), 1);
        let line = err.line("start").expect("the refusal speaks");
        assert!(
            line.contains("Failed to send message: connection failed"),
            "the carrier's own sentence is carried verbatim: {line}"
        );
        assert!(
            line.contains("IS RUNNING"),
            "an exit 1 from a CREATE verb must not read as \"nothing happened\": {line}"
        );
    }

    // --- the W8 verify ruling ----------------------------------------------

    /// W8's chunked read-back does NOT run on the relay path: nothing is chunked,
    /// a partial body is a refusal rather than a silent short write, and the
    /// transcript record is `<channel …>`-wrapped so the exact-match verify could
    /// only ever degrade to a WARNING about a delivery that succeeded.
    ///
    /// MUTATION EVIDENCE: making `payload_verify_applies` ignore the carrier REDs
    /// the first assert.
    #[test]
    fn the_chunked_verify_is_pane_only() {
        let chunked = "x".repeat(events::CHUNK_BYTES * 2);
        assert!(
            !payload_verify_applies(ClaudeCarrier::Relay { port: 4711 }, &chunked),
            "a relay body is atomic and receipt-closed — nothing to read back"
        );
        assert!(
            payload_verify_applies(ClaudeCarrier::MuxPty, &chunked),
            "the pane path is byte-for-byte unchanged"
        );
        assert!(
            !payload_verify_applies(ClaudeCarrier::MuxPty, "short"),
            "a single-chunk pane send never verified and still does not"
        );
    }

    /// The budget is priming's own [`DELIVER_TIMEOUT_S`], measured from BEFORE the
    /// POST, and it never goes negative.
    ///
    /// MUTATION EVIDENCE: dropping the `.max(0)` REDs the third assert; using the
    /// relay carrier's own default instead of DELIVER_TIMEOUT_S REDs the first.
    #[test]
    fn the_receipt_wait_spends_primings_own_budget() {
        assert_eq!(remaining_budget_ms(0, 0), DELIVER_TIMEOUT_S as i64 * 1000);
        assert_eq!(remaining_budget_ms(1_000, 1_500), 14_500);
        assert_eq!(remaining_budget_ms(0, 99_000), 0);
    }

    // --- the body, end to end ----------------------------------------------

    /// A relay prime that is RECEIPTED: the send-initiated keeps the priming
    /// send's identity (`verb: "new-p"`, the caller's id, ONE chunk, no transcript
    /// anchor), the relay carrier's own transport pair lands beside it under the
    /// relay-minted id, and the observed receipt reduces to `Accepted` — the
    /// caller's exit 0.
    ///
    /// MUTATION EVIDENCE: stamping `send:relay` (or `send:pty`) as the verb REDs
    /// the identity asserts; typing the payload into the pane instead REDs
    /// `relayed` (the fake relay would never be called).
    #[test]
    fn a_receipted_relay_prime_is_accepted_and_keeps_the_new_p_identity() {
        let jail = Jail::new("sid-1", "wk");
        // The receipt the relay server's observer would have written.
        jail.write_receipt("mid-1");
        let relay = FakeRelay::ok("mid-1");
        let primed = match jail.prime(&relay, "hello prompt") {
            Ok(p) => p,
            Err(_) => panic!("no refusal"),
        };

        assert_eq!(primed.deliver, DeliverOutcome::Accepted);
        assert_eq!(relay.sent(), vec!["hello prompt".to_string()], "relayed");

        let recs = jail.records();
        let ours = recs
            .iter()
            .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(jail.send_id()))
            .expect("the priming send-initiated");
        assert_eq!(ours.str_field("verb").as_deref(), Some("new-p"));
        assert_eq!(ours.str_field("send_path").as_deref(), Some("relay"));
        assert_eq!(ours.u64_field("chunks"), Some(1));
        assert!(
            ours.str_field("transcript").is_none(),
            "a relay send carries no transcript anchor"
        );
    }

    /// A relay prime whose receipt never appears inside the budget is `Stalled` —
    /// the caller's exit 10. The message DID cross (the fake relay accepted it);
    /// what is missing is the child's confirmation.
    ///
    /// MUTATION EVIDENCE: treating a delivered-but-unseen relay send as Accepted
    /// REDs this row.
    #[test]
    fn a_relay_prime_with_no_receipt_is_stalled() {
        let jail = Jail::new("sid-1", "wk");
        let relay = FakeRelay::ok("mid-1");
        let primed = match jail.prime(&relay, "hello prompt") {
            Ok(p) => p,
            Err(_) => panic!("no refusal"),
        };
        assert_eq!(primed.deliver, DeliverOutcome::Stalled);
    }

    /// A receipt for a DIFFERENT message id is not ours: the join is the
    /// relay-minted id, so a neighbour's terminal cannot close our prime.
    ///
    /// MUTATION EVIDENCE: joining on "any terminal in the log" REDs this row.
    #[test]
    fn a_foreign_receipt_does_not_close_our_prime() {
        let jail = Jail::new("sid-1", "wk");
        jail.write_receipt("someone-elses-mid");
        let relay = FakeRelay::ok("mid-1");
        let primed = match jail.prime(&relay, "hello prompt") {
            Ok(p) => p,
            Err(_) => panic!("no refusal"),
        };
        assert_eq!(primed.deliver, DeliverOutcome::Stalled);
    }

    /// A refused POST is the exit-1 arm, and it is a REFUSAL (not a `Primed`), so
    /// the caller never runs it through `map_deliver_outcome` at all.
    ///
    /// MUTATION EVIDENCE: mapping a refusal onto `Stalled` REDs this row.
    #[test]
    fn a_refused_post_refuses_with_exit_one() {
        let jail = Jail::new("sid-1", "wk");
        let relay = FakeRelay::err(RelayError::ConnectionFailed);
        let refused = match jail.prime(&relay, "hello prompt") {
            Err(e) => e,
            Ok(_) => panic!("a refused POST is a refusal"),
        };
        assert_eq!(refused.error.exit_code(), 1);
        assert_eq!(
            refused.message_id.as_deref(),
            Some(jail.send_id()),
            "the refusal stays KEYED — its send-initiated is already durable"
        );
    }

    /// NO SIDECAR ⇒ the pane body runs, unchanged. The fake relay is never
    /// touched, and the send-initiated is the pty path's own
    /// (`send_path: "idle"`).
    ///
    /// MUTATION EVIDENCE: making `relay_target` answer a port without a sidecar
    /// (or defaulting the carrier to Relay) REDs both asserts.
    #[test]
    fn no_sidecar_falls_back_to_the_pane_body() {
        let jail = Jail::without_sidecar("sid-1", "wk");
        let relay = FakeRelay::ok("mid-1");
        let _ = jail.prime(&relay, "hello prompt");
        assert!(relay.sent().is_empty(), "the relay was never reached");
        let recs = jail.records();
        let ours = recs
            .iter()
            .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(jail.send_id()))
            .expect("the priming send-initiated");
        assert_eq!(
            ours.str_field("send_path").as_deref(),
            Some("idle"),
            "the pane path is untouched by this change"
        );
    }

    // --- the fixtures ------------------------------------------------------

    fn record(line: &str) -> EventRecord {
        parse_events(line)
            .records
            .into_iter()
            .next()
            .expect("one record")
    }

    /// A `RelayContract` that answers from a script and records what it was asked
    /// to send. Only `send_message` is ever reached on this path.
    struct FakeRelay {
        answer: Result<String, RelayError>,
        sent: std::cell::RefCell<Vec<String>>,
    }

    impl FakeRelay {
        fn ok(id: &str) -> Self {
            FakeRelay {
                answer: Ok(id.to_string()),
                sent: std::cell::RefCell::new(Vec::new()),
            }
        }
        fn err(e: RelayError) -> Self {
            FakeRelay {
                answer: Err(e),
                sent: std::cell::RefCell::new(Vec::new()),
            }
        }
        fn sent(&self) -> Vec<String> {
            self.sent.borrow().clone()
        }
    }

    impl RelayContract for FakeRelay {
        fn send_message(&self, _port: u16, text: &str, _from: &str) -> Result<String, RelayError> {
            self.sent.borrow_mut().push(text.to_string());
            self.answer.clone()
        }
        fn fetch_reply(&self, _p: u16, _id: &str, _t: u64) -> Result<RelayReply, RelayError> {
            unreachable!("the priming send never long-polls for a reply")
        }
        fn health(&self, _p: u16, _t: u64) -> Result<RelayHealth, RelayError> {
            unreachable!("the port is resolved from the sidecar, never probed")
        }
    }

    /// A clock that ADVANCES on every read. `watch_terminal` bounds itself on
    /// elapsed time, so a fixed clock would spin forever on the no-receipt row;
    /// this is the smallest thing that makes the budget genuinely expire.
    struct TickClock(Cell<i64>);
    impl Clock for TickClock {
        fn now_ms(&self) -> i64 {
            let v = self.0.get();
            self.0.set(v + 1_000);
            v
        }
    }

    struct NoSleep;
    impl Sleeper for NoSleep {
        fn sleep_ms(&self, _ms: u64) {}
    }

    /// A HOME-jailed session: registry row, optional relay sidecar, and the state
    /// dir both halves of the ledger live under.
    struct Jail {
        home: tempfile::TempDir,
        session_id: String,
        name: String,
        send_id: String,
    }

    impl Jail {
        fn build(session_id: &str, name: &str, sidecar: bool) -> Self {
            let home = tempfile::tempdir().unwrap();
            let paths = QdPaths::from_home(home.path());
            std::fs::create_dir_all(&paths.sessions_dir).unwrap();
            std::fs::write(
                paths.sessions_dir.join("4242.json"),
                format!(
                    r#"{{"pid":4242,"status":"idle","name":"{name}","sessionId":"{session_id}"}}"#
                ),
            )
            .unwrap();
            if sidecar {
                std::fs::create_dir_all(&paths.relay_dir).unwrap();
                std::fs::write(
                    paths.relay_dir.join("relay-1.json"),
                    format!(r#"{{"port":4711,"sessionId":"{session_id}","pid":4243}}"#),
                )
                .unwrap();
            }
            Jail {
                home,
                session_id: session_id.to_string(),
                name: name.to_string(),
                send_id: "1-2-3".to_string(),
            }
        }
        fn new(session_id: &str, name: &str) -> Self {
            Self::build(session_id, name, true)
        }
        fn without_sidecar(session_id: &str, name: &str) -> Self {
            Self::build(session_id, name, false)
        }

        fn send_id(&self) -> &str {
            &self.send_id
        }

        fn env(&self) -> MapEnv {
            let mut vars = HashMap::new();
            vars.insert(
                "HOME".to_string(),
                self.home.path().to_string_lossy().into_owned(),
            );
            MapEnv { vars, uid: 501 }
        }

        fn state_dir(&self) -> std::path::PathBuf {
            QdPaths::from_home_env(self.home.path(), &self.env()).state_dir
        }

        /// Pre-write the `message-seen` the relay server's received-observer would
        /// have written for `mid` — same writer, same key, same payload.
        fn write_receipt(&self, mid: &str) {
            let writer = EventWriter::for_key(
                &self.state_dir(),
                &self.session_id,
                Some(self.session_id.clone()),
                Some(self.name.clone()),
            );
            events::warn_emit(
                &writer,
                &FixedClock(1_770_000_000_000),
                &Payload::MessageSeen {
                    send_id: mid.to_string(),
                    content_sha256: events::sha256_hex(b"hello prompt"),
                },
            );
        }

        fn records(&self) -> Vec<EventRecord> {
            events::read_merged(
                &self.state_dir(),
                Some(&self.session_id),
                Some(&self.name),
            )
            .records
        }

        fn prime(
            &self,
            relay: &dyn RelayContract,
            prompt: &str,
        ) -> Result<Primed, Refused<PrimingError>> {
            let env = self.env();
            let clock = TickClock(Cell::new(1_770_000_000_000));
            let sleeper = NoSleep;
            let mux = FixtureMux::new();
            let paths = QdPaths::from_home(self.home.path());
            let deps = PrimingDeps {
                env: &env,
                clock: &clock,
                sleeper: &sleeper,
                mux: &mux,
                paths: &paths,
                home: self.home.path(),
                socket_dir: self.home.path(),
            };
            let params = PrimingParams {
                name: &self.name,
                prompt,
                session_id: Some(&self.session_id),
                send_id: &self.send_id,
            };
            prime_with_relay(&deps, &params, relay)
        }
    }
}

#[cfg(test)]
mod tests {
    //! §5.1 / G3 — the `priming-readiness-timeout` rows, moved here 1:1 from
    //! `bin/qd/verbs/lifecycle.rs` with their emitter. Nothing about what they
    //! assert changed; the argument order did (`env, clock, home` — the crate's
    //! deps-then-data convention) and the paths now name this crate.
    //!
    //! The emission is the reachable unit seam (the full create-boot path is
    //! jail-only, M5/G7c). The mutation control: deleting the `warn_emit` in
    //! [`emit_priming_timeout`] REDs all three.

    use super::*;
    use crate::boot::BootPhase;
    use crate::create::NewError;
    use crate::effects::{RealClock, RealEnv};
    use crate::events::{byname_key, parse_events};

    /// The byname events file emit_priming_timeout writes to, resolved the SAME
    /// way the function does (QD_HOME-honoring) so the test is hermetic.
    fn byname_events_file(home: &std::path::Path, name: &str) -> std::path::PathBuf {
        let state = crate::paths::QdPaths::from_home_env(home, &RealEnv).state_dir;
        crate::events::events_path(&state, &byname_key(name))
    }

    #[test]
    fn priming_timeout_pid_file_phase_emits_to_byname() {
        let home = tempfile::tempdir().unwrap();
        // m-4 (ack3-spec §8): keyed on the TYPED phase, not a detail string.
        emit_priming_timeout(&RealEnv, &RealClock, home.path(), "wk", BootPhase::PidFile);
        let file = byname_events_file(home.path(), "wk");
        let text = std::fs::read_to_string(&file).expect("byname events file written");
        let recs = parse_events(&text).records;
        assert_eq!(recs.len(), 1, "exactly one record");
        assert_eq!(recs[0].event, "priming-readiness-timeout");
        assert_eq!(recs[0].str_field("phase").as_deref(), Some("pid-file"));
        // waited_ms = the pid-phase default (40s); best-effort.
        assert_eq!(recs[0].u64_field("waited_ms"), Some(40_000));
        // No sessionId on a failed boot → keyed by name only.
        assert_eq!(recs[0].name.as_deref(), Some("wk"));
        assert!(recs[0].session.is_none());
    }

    #[test]
    fn priming_timeout_idle_phase_typed() {
        let home = tempfile::tempdir().unwrap();
        // m-4 (ack3-spec §8): the Idle phase is read TYPED, not parsed from wording.
        emit_priming_timeout(&RealEnv, &RealClock, home.path(), "wk", BootPhase::Idle);
        let file = byname_events_file(home.path(), "wk");
        let text = std::fs::read_to_string(&file).unwrap();
        let recs = parse_events(&text).records;
        assert_eq!(recs[0].str_field("phase").as_deref(), Some("idle"));
        assert_eq!(recs[0].u64_field("waited_ms"), Some(60_000));
    }

    /// m-4 REGRESSION TOOTH (ack3-spec §8): a BootTimeout whose detail string is
    /// REWORDED — it does NOT contain "did not reach idle" — but whose TYPED phase
    /// is `Idle` still files the event as "idle". The deleted string-match would
    /// have misfiled this as "pid-file" (the exact brittleness m-4 removes). We
    /// drive through the create-path destructure the real consumer uses, so the
    /// phase flows the same way production threads it.
    #[test]
    fn priming_timeout_reworded_idle_detail_still_files_idle() {
        let err = NewError::BootTimeout {
            name: "wk".to_string(),
            phase: BootPhase::Idle,
            // Deliberately REWORDED: no "did not reach idle" substring.
            detail: "session never settled to idle".to_string(),
        };
        let NewError::BootTimeout { phase, detail, .. } = &err else {
            panic!("constructed a BootTimeout");
        };
        // Guard the premise: the old string-match key is genuinely absent.
        assert!(!detail.contains("did not reach idle"));

        let home = tempfile::tempdir().unwrap();
        emit_priming_timeout(&RealEnv, &RealClock, home.path(), "wk", *phase);
        let file = byname_events_file(home.path(), "wk");
        let text = std::fs::read_to_string(&file).unwrap();
        let recs = parse_events(&text).records;
        // Typed phase wins: filed as "idle" despite the reworded detail.
        assert_eq!(recs[0].str_field("phase").as_deref(), Some("idle"));
        assert_eq!(recs[0].u64_field("waited_ms"), Some(60_000));
    }
}
