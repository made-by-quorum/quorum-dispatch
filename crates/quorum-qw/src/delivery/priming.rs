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
//! - **Not [`LaneOps::deliver`](crate::contract::LaneOps::deliver).** That method
//!   picks its carrier with `claude_carrier`, so a session with a recorded relay
//!   port would be primed over HTTP instead of typed into its pane; it stamps
//!   `verb: "send:pty"` where the recovery sweep looks for `"new-p"`; its budget
//!   is the pane carrier's, not `DELIVER_TIMEOUT_S`; and it mints terminals on
//!   failure arms this path deliberately leaves open (see the `DeliverOutcome`
//!   discriminator below). Every one of those is a behaviour change.
//!
//! So the body moved by OWNERSHIP rather than by address, the way phase 3B moved
//! the other five: a core here that answers, a `qd start` wrapper that prints and
//! chooses an exit code. What stayed in the verb is exactly what is qd's — the
//! INTENT record (ruling D11), the `map_deliver_outcome` exit contract, and the
//! stderr lines this core returns as [`Notes`] or as [`PrimingError`].

use std::cell::Cell;
use std::path::{Path, PathBuf};

use crate::boot::Sleeper;
use crate::delivery::pty::{
    ack_source_label, emit_w8_anchored, emit_w8_mismatch, send_text_chunked_mux,
};
use crate::delivery::{CarrierError, Notes, Refused};
use crate::effects::{Clock, Env};
use crate::events::{self, EventWriter, Payload, WatchGuard};
use crate::mux::Mux;
use crate::paths::QdPaths;
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
        }
    }

    fn exit_code(&self) -> i32 {
        1
    }
}

// ===========================================================================
// The body
// ===========================================================================

/// Deliver `qd start -p`'s first turn into the freshly-booted session's pane.
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
pub fn prime_new_session(
    deps: &PrimingDeps,
    params: &PrimingParams,
) -> Result<Primed, Refused<PrimingError>> {
    let name = params.name;
    let p = params.prompt;
    let send_id = params.send_id;
    let mut notes = Notes::new();

    // --- ACK-2 §9 (M3): engine event emission for the -p send (best-effort) ---
    // events key: sessionId if resolvable NOW (the existing non-blocking registry
    // read), else byname(name) — the key choice is STICKY for ALL of this send's
    // events (§4.1). state_dir honors QD_HOME (§4.1 / ADD-14).
    let ev_state = QdPaths::from_home_env(deps.home, deps.env).state_dir;
    let ev_session_id = params.session_id.map(str::to_string);
    let ev_key = ev_session_id
        .clone()
        .unwrap_or_else(|| events::byname_key(name));
    let writer = EventWriter::for_key(
        &ev_state,
        &ev_key,
        ev_session_id.clone(),
        Some(name.to_string()),
    );

    // D10 (R6/R7): the transcript+offset snapshot is UNCONDITIONAL-when-resolvable
    // (not only when payload_needs_verify). The verify step still uses the SAME
    // offset; a single-chunk send simply doesn't run verify.
    let ev_transcript = resolve_new_p_transcript(deps.paths, name);
    let snapshot_offset: u64 = ev_transcript
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok().map(|m| m.len()))
        .unwrap_or(0);
    // The verify window offset = the snapshot when verify runs, else unused.
    let verify_offset: u64 = if payload_needs_verify(p) {
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
        &writer,
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
    let guard = WatchGuard::arm(&writer, &clock_ref, send_id);

    let outcome = deliver_prompt(&deliver, p, DELIVER_TIMEOUT_S);

    // §2.3.2 chunks-delivered: all text chunks acked → emit.
    let acks_total = deliver.total.get();
    let acks_acked = deliver.acked.get();
    if acks_total > 0 && acks_acked == acks_total {
        events::warn_emit(
            &writer,
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
    if outcome == DeliverOutcome::Accepted && payload_needs_verify(p) {
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
                    &writer,
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
                });
            }
            PayloadVerifyOutcome::Truncated { expected, recorded } => {
                // §2.3.4 anchored-mismatch (terminal): the lengths come from the
                // outcome; actual_sha is re-derived by re-reading past the offset +
                // sha-ing the longest truncation signature. Emitted BEFORE the
                // unchanged exit-1 the caller renders.
                emit_w8_mismatch(
                    &writer,
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
    })
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
