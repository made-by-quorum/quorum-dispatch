//! REAL `send:pty` + `send:http` backends (spec §3.1, §3.3).
//!
//! `send:relay` is M3's surface (not here). This file is the `--wait` half of
//! `send:pty`, and nothing else.
//!
//! ── WHAT USED TO BE HERE ────────────────────────────────────────────────────
//! `run_send_pty_resolved` — ~1100 lines of PTY delivery with 41 `eprintln!` and
//! 6 `println!` threaded through 25 return sites — was the fifth and last
//! `quorum_qw::Carriers` callback: `LaneOps::deliver` reached UP out of the `qw`
//! library into this verb to actually send. Stage-3 phase 3B moved it to
//! [`quorum_qw::delivery::pty`], and with it the trait, `RealUnifiedBackend` and
//! `lane_ops_with_carriers` are gone — no `qw` code calls into the `qd` binary.
//!
//! The cut is at the `wait`/`raw`/`full` boundary rather than the function
//! boundary, because `--wait`'s printing is not line-shaped: `eprint!("Waiting
//! for response")` opens a line, a progress glyph is written per 500ms poll, and
//! `eprintln!(" done")` closes it 120 seconds later. A library that answers
//! cannot do that, so the delivery half answers and THIS half prints. The seam is
//! [`quorum_qw::delivery::pty::PtyOutcome::Await`], which carries everything the
//! two `--wait` phases below need and nothing they could re-derive.
//!
//! ── AND THE LEDGER HALF OF THE WAIT WENT TOO ───────────────────────────────
//! 3B left three emissions on this side — `turn-anchored`, `status-transition`
//! and the `WatchGuard`'s Drop terminal — along with the `EventWriter` this file
//! rebuilt from `PtyAwait::ev_paths` to close the send the delivery half armed.
//! Those were the last qw-owned ledger records written from a qd verb, and they
//! followed the carrier into [`quorum_qw::delivery::pty::run_anchor_wait`] (and
//! its write-free embedded twin `run_reply_capture`).
//!
//! The cut did NOT move: the banner, the five closing words and the stdout body
//! are still written here, for exactly the reason above. What crossed is the
//! WRITING, which was never line-shaped at all. So this file now emits nothing,
//! constructs no `Payload`, and names neither log — it prints an answer and picks
//! an exit code, which is the whole of what a verb wrapper does.

use clap::ArgMatches;

use dispatch::boot::RealSleeper;
use dispatch::effects::{RealClock, RealEnv};
use dispatch::sendpty::ExtractMode;
use quorum_qw::contract::{LaneOps, MessageId, SessionId, Terminal};
use quorum_qw::delivery::pty::{
    self, PtyAwait, PtyDeps, PtyOutcome, PtyParams, ReplyOutcome, ReplyParams, WaitPhase,
};
use quorum_qw::delivery::render_notes;

use super::common;

/// Append a content-free A6 invoked line for a SUCCESSFUL `send` (any exit-0 path).
/// Best-effort: a failure warns but NEVER changes the verb's exit code (spec
/// §4.1 — telemetry must never break a working send).
///
/// The DELIVERY half's copy of this is [`quorum_qw::delivery::pty::append_pty_invoked`],
/// which returns the warning as a note instead of printing it. This one prints,
/// because its two callers are already printing mid-line when they reach it.
fn invoked_send(session_id: &str, name: Option<&str>) {
    if let Err(e) =
        dispatch::telemetry::append_invoked(&RealEnv, &RealClock, "send", Some(session_id), name)
    {
        eprintln!("WARNING: telemetry invoked append failed (non-fatal): {e}");
    }
}

// ===========================================================================
// send:pty (a4-spec §3.1)
// ===========================================================================

/// `qd send:pty <session> <message>` — REAL (replaces the A4 stub). Port of the
/// send:pty action (qa/hardening@3dd9f1e:src/commands/send.ts:100-336).
pub fn run_send_pty(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let message = m.get_one::<String>("message").expect("required by clap");
    let wait = m.get_flag("wait");
    let raw = m.get_flag("raw");
    let full = m.get_flag("full");
    let timeout = m
        .get_one::<String>("timeout")
        .map(String::as_str)
        .unwrap_or("120");

    // Resolve through the sealed uncapped entry (was getAllSessions + resolveOrDie,
    // send.ts:101-103). D-2 reject-set: a stopped pane can't receive a send, so a
    // tombstone is FOUND then rejected with the clear "resume it first" message.
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };
    if let Err(code) = common::reject_if_tombstoned(query, &session) {
        return code;
    }
    run_send_pty_resolved(&session, message, wait, raw, full, timeout)
}

/// Deliver through [`quorum_qw::delivery::pty`], then print its answer — and, on
/// `--wait`, keep going.
///
/// `strict` is `false` and is not a parameter: the explicit `send:pty` verb keeps
/// its historic warning+exit-0 contract on a known-failed submission, and the only
/// caller that ever wanted `strict=true` was the unified `qd send`, which now
/// reaches the carrier through `LaneOps::deliver` instead of through this file.
fn run_send_pty_resolved(
    session: &dispatch::model::Session,
    message: &str,
    wait: bool,
    raw: bool,
    full: bool,
    timeout: &str,
) -> i32 {
    let env = RealEnv;
    let clock = RealClock;
    let sleeper = RealSleeper;
    let deps = PtyDeps {
        env: &env,
        clock: &clock,
        sleeper: &sleeper,
    };
    // WRITE-THEN-DELIVER (`09-ledger-split.md`). The id is minted and qd's intent
    // record is durable BEFORE the pane carrier types a byte, because the send
    // this verb has to survive is the one whose writer is killed mid-delivery —
    // and `qd delivery:recover`'s sweep can only find what qd already wrote.
    // `verb_str(false)` is the same `"send:pty"` token the carrier stamps on its
    // own record, so the two halves of this send agree on both id and verb.
    let send_id = super::intent::record_send_intent(
        &env,
        &clock,
        Some(&session.session_id),
        session.name.as_deref(),
        dispatch::events::verb_str(false),
        message,
    );
    let params = PtyParams {
        session,
        message,
        send_id: &send_id,
        wait,
        strict: false,
    };
    let awaiting = match pty::send_mux_pty(&deps, &params) {
        // The delivery half is finished — its notes, its stdout receipt and its
        // refusal line all print through the ONE shared renderer, the same one
        // `LaneOps::deliver`'s four daemon arms and its two pane arms use.
        Ok(PtyOutcome::Done(d)) => {
            return quorum_qw::delivery::render::<pty::MuxPtyError>(Ok(d), "send:pty").code
        }
        Err(r) => return quorum_qw::delivery::render(Err(r), "send:pty").code,
        Ok(PtyOutcome::Await(a)) => a,
    };
    // Notes first, exactly where the pre-move body wrote them: before the first
    // mid-line byte of the wait banner.
    render_notes(&awaiting.notes);
    match awaiting.phase {
        WaitPhase::EmbeddedTerminal => {
            wait_embedded_terminal(session, &awaiting, message, timeout, raw, full, &deps)
        }
        WaitPhase::AnchorLoop => {
            wait_anchor_loop(session, &awaiting, message, timeout, raw, full, &deps)
        }
    }
}

/// The `ExtractMode` the `--raw` / `--full` flags select. The one thing on this
/// side of the cut that those two flags decide, and the reason neither is a
/// parameter of the delivery half.
fn extract_mode(raw: bool, full: bool) -> ExtractMode {
    if raw {
        ExtractMode::Raw
    } else if full {
        ExtractMode::Full
    } else {
        ExtractMode::Default
    }
}

/// `qd send:pty --wait` under the EMBEDDED backend: await a REAL terminal.
///
/// The mux owns the send's lifecycle from the `DeliveryQueued` receipt onward, so
/// the question "did this land" is the LANE's, not a file read's. Everything
/// below was `run_send_pty_resolved`'s `--wait` arm verbatim; what changed is
/// where its inputs come from — [`PtyAwait`], filled in by the delivery half.
#[allow(clippy::too_many_arguments)]
fn wait_embedded_terminal(
    session: &dispatch::model::Session,
    a: &PtyAwait,
    message: &str,
    timeout: &str,
    raw: bool,
    full: bool,
    deps: &PtyDeps,
) -> i32 {
    let label = &a.label;
    // --wait: await a REAL 7-set terminal (NEVER return on the non-terminal
    // DeliveryQueued ack — a queued ack is NOT delivery).
    //
    // This USED to say "READ-ONLY: qd emits nothing", and that claim moved
    // with the read. Stage 3A replaced qd's own ledger poll with
    // `LaneOps::await_terminal`, and the lane answers a budget it exhausts by
    // EMITTING a positive `anchor-timeout` (`events::await_received`'s §8
    // contract, "timeouts stay positive events"). So on the exhaustion path a
    // terminal is now written — by the LANE, into the lane's own log, which is
    // where the ledger split puts it, but written all the same. qd itself
    // still emits nothing here.
    //
    // THAT IS A LIVE CONFLICT, recorded rather than papered over: an
    // `anchor-timeout` is terminal under first-terminal-wins, so a mux that
    // resolves this send to `message-seen` at 80s loses to a watch that gave
    // up at 75s. It is the same foreclosure `ack2_gate::
    // g3_seq_sendpty_wait_timeout_no_foreclosing_terminal` forbids on the ZMX
    // path, and `Terminal::TimedOut`'s own doc ("says nothing about
    // delivery") disagrees with an emit that says a great deal. Resolving it
    // is a ruling about whether a CLIENT's budget may mint a terminal, not a
    // rendering choice, so it is not resolved here.
    //
    // `--timeout` semantics (F4, explicit contract): `--timeout` bounds the
    // REPLY-CAPTURE phase (the JSONL anchor loop below), NOT the delivery-watch.
    // The delivery-watch uses a FLOOR spanning the mux's full countdown ceiling +
    // fire + landing window (FireConfig defaults ~60s + ~8s ⇒ 75s): a send the mux
    // is legitimately HOLDING through a human's countdown must be awaited to its
    // terminal, not reported "pending" just because a short `--timeout` elapsed
    // (that would be a false-pending on an in-flight polite hold). So the watch
    // bound is `max(--timeout, floor)` — a larger `--timeout` is honored; a smaller
    // one is raised to the floor. qd cannot cheaply observe attendance itself (the
    // mux owns that seam), so the floor is unconditional rather than
    // countdown-gated; the cost is only paid when no terminal appears (a genuinely
    // pending send), and the message below names that honestly.
    const EMBEDDED_WAIT_FLOOR_MS: i64 = 75_000;
    let bound_ms = pty::timeout_ms(timeout).max(EMBEDDED_WAIT_FLOOR_MS);

    // Stage 3A: this is a CALL, not a file read. qd used to answer "did this
    // message land" by opening the mux's `…events.jsonl` itself
    // (`sendpty::watch_terminal` over `events::read_merged`), which made a
    // shared file the wire protocol between the two halves — unversioned,
    // unenumerable, and impossible to know who depends on which field. The
    // question is the lane's, so it is asked of the lane
    // (`LaneOps::await_terminal`, whose own doc says exactly this).
    //
    // The lane comes from the ROW's provider + hosting, the same idiom
    // `attach`/`resume`/`kill` already use. `lane_for` returning `None` means a
    // genuinely unknown provider, which `MuxPtyError::UnknownProvider` already
    // refused in the delivery half — so it is unreachable here, and spelled out
    // rather than `unwrap`ed because an unreachable arm that panics is a worse
    // answer than one that refuses. The wording is `common`'s, so the two
    // refusals stay one sentence.
    let Some(lane) = quorum_qw::lane_for(&session.provider, session.hosting.as_deref()) else {
        return common::refuse_unknown_provider("send:pty", session).unwrap_or(1);
    };
    // `lane_ops` — the only constructor there is. `await_terminal` reads the
    // ledger and (on a dead-dangling send) the recipient's transcript; the
    // ledger it reads is the one the delivery half wrote, which is why
    // `ev_paths` travels on `PtyAwait` rather than being re-derived.
    let ops = dispatch::lane::open(lane, deps.env, a.ev_paths.clone());
    eprint!("Waiting for delivery");
    {
        use std::io::Write;
        let _ = std::io::stderr().flush();
    }
    let verdict = ops.await_terminal(
        &SessionId(session.session_id.clone()),
        &MessageId(a.send_id.clone()),
        // The contract's budget is unsigned wall-clock ms; `timeout_ms`
        // returns i64 and the floor above is positive, so the clamp is a
        // formality that cannot move the bound.
        bound_ms.max(0) as u64,
    );
    match verdict {
        // F5 (shared success-terminal identity): "which terminal means
        // delivered" stays the leaf crate's ONE definition
        // (`is_success_terminal`) — it is now applied on the LANE's side of
        // the call, inside `events::received_from_terminal`, and arrives here
        // as `Terminal::Seen`. Still no locally-minted "message-seen" success
        // literal on either side (M4 F5/M2 fold); the mux banner
        // (qrmux/attended/driver.rs `toast_kind_for`) reads the same one home.
        Ok(Terminal::Seen) => {
            eprintln!(" delivered");
            // QS-7: preserve the user-facing --wait reply print via the EXISTING
            // JSONL anchor loop — WITHOUT emitting any terminal (the mux already
            // emitted the one terminal; single-writer) and WITHOUT the WatchGuard
            // or the status-transition emitter (`status_emit: None`): qd writes
            // NOTHING to the ledger on this path.
            let Some(jp) = a.jsonl_path.clone() else {
                // Delivery is confirmed (message-seen); we just can't read the
                // reply back without a transcript. Report the confirmed success.
                println!(
                    "Message delivered to \"{label}\" (reply not captured: transcript unavailable)"
                );
                invoked_send(&session.session_id, session.name.as_deref());
                return 0;
            };
            // The write-free twin: the mux already emitted this send's ONE
            // terminal, so the capture below runs with no writer, no WatchGuard
            // and no status emitter. qd writes NOTHING to the ledger on this path.
            let outcome = pty::run_reply_capture(
                deps,
                a,
                &ReplyParams {
                    session,
                    message,
                    jsonl_path: &jp,
                    timeout_ms: pty::timeout_ms(timeout),
                    mode: extract_mode(raw, full),
                },
            );
            invoked_send(&session.session_id, session.name.as_deref());
            return match outcome {
                ReplyOutcome::Complete { capture } => {
                    eprintln!(" done");
                    match capture {
                        Ok(body) => {
                            println!("{body}");
                            0
                        }
                        Err(observed) => {
                            eprintln!("--wait capture EMPTY: {observed}.");
                            eprintln!(
                                "The reply may still be flushing to the transcript — read it \
                                 directly to recover it: {}",
                                jp.display()
                            );
                            1
                        }
                    }
                }
                // Delivery was ALREADY confirmed by the mux terminal; a
                // reply-collection miss is a capture failure, not a delivery lie.
                // Exit non-zero (parity with today's --wait), but the message names
                // that the send DID land — never a false "not delivered".
                ReplyOutcome::Died => {
                    eprintln!(" session died");
                    eprintln!("Delivered to \"{label}\", but the session exited before its reply completed.");
                    1
                }
                ReplyOutcome::TimedOut { .. } => {
                    eprintln!(" timeout");
                    eprintln!("Delivered to \"{label}\", but timed out waiting for the reply.");
                    1
                }
                ReplyOutcome::SourceError(reason) => {
                    eprintln!(" error");
                    eprintln!(
                        "Delivered to \"{label}\", but the transcript integrity was lost while \
                         reading the reply: {reason}"
                    );
                    1
                }
            };
        }
        // A mux failure/mismatch terminal. Honest failure, named from the
        // ledger; no reply collection.
        //
        // The parenthetical used to be `term.event` — the raw ledger tag —
        // and it is reproduced here rather than reworded, because it is what
        // an operator greps the ledger for. `Mismatch` carries no payload
        // (there is exactly one event behind it), so its tag is written out;
        // `NotDelivered.reason` already IS the ledger's own token, minted by
        // `lanes::terminal_from_received` — `pending-abandoned` verbatim, and
        // `send-failed`/`seen-failed` with their `reason` field appended,
        // which is strictly more than the bare tag said.
        Ok(Terminal::Mismatch) => {
            eprintln!(" failed");
            eprintln!(
                "Delivery to \"{label}\" did not land (turn-anchored-mismatch) — check: qd attach {label}"
            );
            return 1;
        }
        Ok(Terminal::NotDelivered { reason }) => {
            eprintln!(" failed");
            eprintln!("Delivery to \"{label}\" did not land ({reason}) — check: qd attach {label}");
            return 1;
        }
        // Bound elapsed with NO terminal, or the lane looked and could not
        // tell. The mux left the send Pending (e.g. a busy-queued turn landing
        // past the mux's landing window) or it is still counting down under
        // continuous typing. HONEST still-pending — never a false "landed",
        // never a false failure (invariant 1). The send is spooled in the mux
        // and resolves later (reconcile / a later landing).
        //
        // `TimedOut` and `Undetermined` share this arm deliberately: both mean
        // "no verdict yet", which is the one thing this line says. Neither may
        // become the failure arm above — foreclosing a send the ledger never
        // foreclosed is the "the ledger lies" failure the whole apparatus
        // exists to prevent (`terminal_from_received`'s own note).
        Ok(Terminal::TimedOut) | Ok(Terminal::Undetermined { .. }) => {
            eprintln!(" pending");
            eprintln!(
                "Delivery to \"{label}\" is still pending (accepted by the mux, not yet \
                 confirmed landed) — it will resolve in the mux; check: qd attach {label}"
            );
            return 1;
        }
        // `LaneImpl::await_terminal` returns `Ok` on every path today, so this
        // is the honest handling of a refusal it does not yet produce — and it
        // is an un-read, not a non-delivery, so it renders as pending rather
        // than as a failure.
        Err(e) => {
            eprintln!(" pending");
            eprintln!(
                "Delivery to \"{label}\" could not be confirmed ({e}) — check: qd attach {label}"
            );
            return 1;
        }
    }
}

/// `qd send:pty --wait` under the ZMX backend: the JSONL anchor loop.
///
/// The reply-capture half, verbatim from `run_send_pty_resolved`'s tail. It is
/// the reason the cut is where it is: `eprint!("Waiting for response")` opens a
/// stderr line, [`RealWaitDeps::progress`] writes a glyph into it every 500ms,
/// and one of five arms closes it — none of which a function that ANSWERS can do.
#[allow(clippy::too_many_arguments)]
fn wait_anchor_loop(
    session: &dispatch::model::Session,
    a: &PtyAwait,
    message: &str,
    timeout: &str,
    raw: bool,
    full: bool,
    deps: &PtyDeps,
) -> i32 {
    // --- --wait JSONL anchor loop (both paths converge, send.ts:208-265) -----
    let jp = a
        .jsonl_path
        .clone()
        .expect("jsonl_path set whenever wait is true");

    eprint!("Waiting for response");
    use std::io::Write;
    let _ = std::io::stderr().flush();

    // The loop, its writer, its status-transition emitter and its WatchGuard are
    // [`quorum_qw::delivery::pty::run_anchor_wait`]'s — the last three were
    // qw-owned ledger records being written from this verb. What is left here is
    // the banner above and the five closing arms below: a stderr line opened
    // before the loop and closed after it is not something a library that ANSWERS
    // can write, which is the whole reason the cut is at this boundary.
    //
    // D8 travelled with them: only the Complete arm emits, and the guard is
    // disarmed unconditionally so its Drop cannot re-mint a foreclosing terminal
    // on the three arms that deliberately write nothing. See that function.
    let outcome = pty::run_anchor_wait(
        deps,
        a,
        &ReplyParams {
            session,
            message,
            jsonl_path: &jp,
            timeout_ms: pty::timeout_ms(timeout),
            mode: extract_mode(raw, full),
        },
    );

    match outcome {
        ReplyOutcome::Died => {
            // §C2 (R5 seam ruling 01KX88WKGP + amend rider 3, red-team finding G): a
            // --wait watch that observes the session die is an IN-BAND-UNDETERMINABLE
            // outcome, NOT a determinate failure — and often PROVABLY LANDED. By this
            // point the send's bytes were fully acked (chunks-delivered emitted) and
            // the unconditional `\r` submitted the accumulated turn; `run_wait_loop`
            // FINDS the anchor first (sendpty.rs:376-378) and only THEN returns Died on
            // an unreadable status (:380-382) — so the message may already be committed
            // to the transcript (committed proof:
            // wait_loop_unreadable_after_anchor_still_died). So this arm must NOT mint a
            // terminal: a `pending-abandoned{session-died}` here would permanently
            // FALSE-FAIL a send that actually landed and (first-terminal-wins) FORECLOSE
            // the recovery-read the verb exists to run — the exact F3/B lie-shape at the
            // Died arm. We fail loud (exit 1; the operator signal below is preserved —
            // the C1 account is the standing send-initiated + the loud synchronous exit +
            // C2's PENDING-closable state) but emit NO terminal: the send stays
            // dead-dangling once the sender exits, and `qd delivery:recover` closes it
            // from the transcript — a disclosed turn-anchored{recovered} if the content
            // landed, else pending-abandoned{recovery-no-candidate}. The unconditional
            // disarm inside `run_anchor_wait` keeps this path from re-minting the
            // terminal via Drop.
            eprintln!(" session died");
            eprintln!("Session exited while waiting for response.");
            1
        }
        ReplyOutcome::TimedOut { anchored } => {
            // §C2 (R5 seam ruling 01KX88WKGP + amend rider 3, red-team finding G): a
            // --wait watch that hits its deadline is IN-BAND-UNDETERMINABLE, not a
            // determinate failure. When `anchored` is true the message PROVABLY LANDED
            // (the anchor was found — sendpty.rs:376-378, :402-404) and the response is
            // merely slow; when un-anchored it is still queued behind the session's
            // current turn ("processed when the turn ends"). Either way the turn may yet
            // commit, so this arm must NOT mint a terminal: an `anchor-timeout` here would
            // FALSE-FAIL a landed-or-still-queued send and (first-terminal-wins) FORECLOSE
            // the recovery-read the verb exists to run — the exact F3/B/G lie-shape at the
            // TimedOut arm. We fail loud (exit 1; the operator signal below is preserved —
            // both `anchored` branches — the C1 account is the standing send-initiated +
            // the loud synchronous exit + C2's PENDING-closable state) but emit NO
            // terminal: the send stays dead-dangling once the sender exits, and
            // `qd delivery:recover` closes it from the transcript. The unconditional
            // disarm inside `run_anchor_wait` keeps Drop from re-minting the terminal.
            eprintln!(" timeout");
            if anchored {
                eprintln!("Timed out waiting for response.");
            } else {
                // Un-anchored: still queued behind the session's current turn —
                // NOT lost (send.ts:255-261 wording verbatim).
                eprintln!(
                    "Timed out — message still queued (session busy with its current task); \
                     it will be processed when the turn ends."
                );
            }
            1
        }
        ReplyOutcome::Complete { capture } => {
            // The `turn-anchored` this arm mints was emitted inside
            // `run_anchor_wait`, before it answered — the ONE arm of the five that
            // writes, and only when the W8 verify has not already anchored the send.
            eprintln!(" done");
            // The SEND landed (anchored) — the A6 invoked line keys on the send,
            // not the capture, so it stays unconditional on this arm even when
            // the capture below fails loud (declared judgment call, B2 item 3).
            invoked_send(&session.session_id, session.name.as_deref());
            // B2 item 3 — the binding capture invariant: --wait NEVER returns
            // an empty capture as success. capture_or_defect Ok = real content
            // (exit 0); Err = a LOUD non-zero exit naming what was observed.
            // SANCTIONED DIVERGENCE from TS (which printed "(no text
            // response)" / nothing and exited 0 — incl. the old empty --raw
            // silent-0 special case).
            match capture {
                Ok(body) => {
                    println!("{body}");
                    0
                }
                Err(observed) => {
                    eprintln!("--wait capture EMPTY: {observed}.");
                    eprintln!(
                        "The reply may still be flushing to the transcript — read it \
                         directly to recover it: {}",
                        jp.display()
                    );
                    1
                }
            }
        }
        ReplyOutcome::SourceError(reason) => {
            // §C2 (R5 seam ruling 01KX88WKGP, red-team finding B): a watch
            // interrupted while confirming turn-completion is an
            // IN-BAND-UNDETERMINABLE outcome, NOT a determinate abandonment. By
            // this point the send's bytes were FULLY acked (chunks-delivered
            // emitted → on the pty) and the unconditional `\r` submitted the
            // accumulated turn; a JSONL integrity loss only means we can no longer
            // OBSERVE whether the turn committed — the turn may well have landed.
            // So this door must NOT mint a terminal: a
            // `pending-abandoned{watch-interrupted}` here would permanently
            // FALSE-FAIL a send that actually landed and (once written) FORECLOSES
            // the recovery-read the verb exists to run — the exact F3 lie-shape at
            // the watch phase (partial-write door, commit 4ed923de). We fail loud
            // (exit 1; the loud operator signal below is preserved — the C1
            // account is the standing send-initiated + the loud synchronous exit +
            // C2's PENDING-closable state) but emit NO terminal: the send stays
            // dead-dangling once the sender exits, and `qd delivery:recover` closes
            // it from the transcript — a disclosed turn-anchored{recovered} if the
            // content landed, else pending-abandoned{recovery-no-candidate}. The
            // WatchGuard mechanism is untouched (the daemon bytes-written path is
            // deferred to C6); the unconditional disarm inside `run_anchor_wait`
            // keeps this path from re-minting the terminal via Drop.
            // ADD-8 W5: integrity loss on the JSONL source — loud, never a
            // silent whole-file re-anchor. Rust-only guard (TS has the silent
            // fallback class; named in the A4 matrix).
            eprintln!(" error");
            eprintln!("Conversation JSONL integrity lost while waiting: {reason}");
            1
        }
    }
}

// ===========================================================================
// send:http — error path REAL, success path parked (a4-spec §3.3)
// ===========================================================================

/// `qd send:http <session> <message>` — engine sessions are NEVER
/// provider=opencode, so this always takes the "not an OpenCode session" ERROR
/// path (send.ts:509-521), exit 1. The OpenCode success path is a named parked
/// exclusion (A3 spec row 10 carries). Flags are parse-accepted (clap) but
/// unused on this path.
pub fn run_send_http(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let _message = m.get_one::<String>("message").expect("required by clap");

    // Resolve through the sealed uncapped entry. D-2 reject-set: a stopped session
    // can't receive a send, so a tombstone is rejected post-resolve with the clear
    // "resume it first" message (even though this verb then always takes the
    // not-opencode error path — resolution stays uniform with the other verbs).
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };
    if let Err(code) = common::reject_if_tombstoned(query, &session) {
        return code;
    }
    let session = &session;

    // Engine sessions are never opencode → the exact TS ERROR block
    // (send.ts:512-520), exit 1.
    let name = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    eprintln!("ERROR: Session '{name}' is not an OpenCode session.");
    eprintln!("send:http only works with OpenCode sessions.");
    eprintln!("  • For Claude Code sessions, use: qd send:relay {query} \"message\"");
    eprintln!("  • Or via PTY: qd send:pty {query} \"message\"");
    1
}
