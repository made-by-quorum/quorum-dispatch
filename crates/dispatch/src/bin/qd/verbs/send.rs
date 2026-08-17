//! REAL `send:pty` + `send:http` backends (spec §3.1, §3.3).
//!
//! `send:relay` is M3's surface (not here). The pure deciders/loop/extraction
//! live in `dispatch::sendpty` (M1 + M2 lib); this file is the thin bin wiring: resolve
//! the session, pin every zmx op to the session's ACTUAL socket dir (Bug D), and
//! drive the queue/idle paths + the `--wait` JSONL-anchor loop with REAL fs/
//! clock/sleep deps.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::boot::{read_pid_status, RealSleeper, Sleeper};
use dispatch::effects::{Clock, RealClock, RealEnv};
use dispatch::events::{self, Anchor, EventWriter, Payload, WatchGuard};
use dispatch::model::SessionStatus;
use dispatch::mux::Mux;
use dispatch::sendpty::{
    composer_holds_message, decide_send_pty, parse_jsonl_slice, run_wait_loop, ExtractMode,
    SendPtyAction, WaitDeps, WaitOutcome,
};
use dispatch::submit::{
    deliver_idle_two_write, send_text_chunked, ChunkSendOptions, IdleDeliverDeps, SubmitOptions,
};
use dispatch::zmx_dir::resolve_zmx_dir;

use super::common;
use super::common::SendBackend;

/// Append a content-free A6 invoked line for a SUCCESSFUL `send` (any exit-0 path).
/// Best-effort: a failure warns but NEVER changes the verb's exit code (spec
/// §4.1 — telemetry must never break a working send).
fn invoked_send(session_id: &str, name: Option<&str>) {
    if let Err(e) =
        dispatch::telemetry::append_invoked(&RealEnv, &RealClock, "send", Some(session_id), name)
    {
        eprintln!("WARNING: telemetry invoked append failed (non-fatal): {e}");
    }
}

/// Per-chunk ack record (ACK-2 §9 recorder, red-team R9): how many text chunks
/// were written and how many `mux.send` calls returned `Ok`. ALL acked
/// (`acked == total`) ⇒ the send's text fully delivered → emit `chunks-delivered`.
///
/// ADD-18 (ack3-spec §5): a partial/total write failure (`!all_ok()`) is now the
/// `send:pty` WRITE-FAILED class — it exits 11 (no longer F1's "discarded
/// result"). `chunks-delivered` stays keyed on `all_ok()`, so a failed write leaves
/// the send-initiated record present and chunks-delivered ABSENT (that file
/// signature is the recovery-read evidence; §5.1). Partial-ack IS failure (message
/// integrity is all-or-nothing).
#[derive(Clone, Copy)]
struct ChunkAcks {
    total: u32,
    acked: u32,
}

impl ChunkAcks {
    fn all_ok(&self) -> bool {
        self.total > 0 && self.acked == self.total
    }
}

/// ADD-18 (ack3-spec §5.1) exit code for a `send:pty` PTY-write failure.
///
/// EXIT-CODE TRIPLE for the send/boot verbs (machine-readable surface, §5.1):
///   1  = generic / infra / arg errors (the broad class)
///   10 = `new -p` Stalled — went-busy acceptance never landed (ADR-0008;
///        lifecycle.rs Stalled path)
///   11 = `send:pty` PTY write failed — one or more chunks' `mux.send` returned
///        Err (immediate daemon Error OR the bounded OP_TIMEOUT); ADD-18.
/// `send:pty` SUCCESS = 0, `--wait` timeout = 1 (unchanged).
const EXIT_PTY_WRITE_FAILED: i32 = 11;

/// ADD-18 (§5.1): the EXACT write-failure stderr line (wording pinned by the
/// contract test). Split from the print/exit so the line is unit-assertable.
fn write_failed_line(acks: ChunkAcks) -> String {
    format!(
        "ERROR: PTY write failed ({}/{} chunks acked) — see events file",
        acks.acked, acks.total
    )
}

/// ADD-18 (§5.1): print the EXACT write-failure stderr line and return exit 11.
/// Called on BOTH send:pty paths when `!acks.all_ok()` after the send stage; it
/// SHORT-CIRCUITS `--wait` (a watch on a failed write is a guaranteed timeout —
/// exiting loud immediately is the honest surface). Events are unchanged at the
/// call site (send-initiated already emitted; chunks-delivered correctly absent —
/// that file signature is the point).
fn write_failed_exit(acks: ChunkAcks) -> i32 {
    eprintln!("{}", write_failed_line(acks));
    EXIT_PTY_WRITE_FAILED
}

/// Send `text` to a session's PTY as ≤1024B code-point-safe chunks with a ~150ms
/// inter-chunk settle (ADR 0009 mode (a): a single large `zmx send` overflows the
/// tty queue and drops wholesale). For text ≤1024B this is a single byte-identical
/// `mux.send`. The SHARED chunking helper drives the per-chunk `mux.send` + sleep.
///
/// ACK-2 §9: RECORDS each chunk's `mux.send` ack (the closure previously discarded
/// it via `let _ =`). ADD-18 (ack3-spec §5) RETIRES the old F1 "discarded result"
/// posture: the recorded tally now DRIVES the exit code — `!all_ok()` after the
/// send stage exits 11 (see `write_failed_exit`). The tally still gates
/// `chunks-delivered` (only a full ack emits it).
fn send_text_chunked_mux(
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
fn ack_source_label() -> &'static str {
    match common::send_backend_label() {
        SendBackend::Embedded => "input-sent",
        SendBackend::Zmx => "cli-exit",
    }
}

/// A bin-local [`IdleDeliverDeps`] that RECORDS each `send_text` chunk's `mux.send`
/// ack (ACK-2 §9 recorder for the idle two-write path; red-team R9). It mirrors
/// `dispatch::submit::RealIdleDeliverDeps` field-for-field EXCEPT `send_text`, which
/// captures `mux.send(...).is_ok()` instead of discarding it (the Real impl does
/// `let _ = mux.send`). This is a NEW bin-local impl — the `IdleDeliverDeps` trait
/// and the pure cores are UNTOUCHED (binding-rule compliant). The CR write
/// (`send_cr`) is NOT counted (only text chunks are chunks-delivered evidence).
struct RecordingIdleDeliverDeps<'a> {
    mux: &'a dyn Mux,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
    zmx_name: String,
    pid_file: PathBuf,
    dir: PathBuf,
    /// Per-chunk ack tally (RefCell — `IdleDeliverDeps::send_text` takes `&self`).
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

/// `parseInt(opts.timeout) * 1000` (send.ts:213). A non-integer default never
/// occurs (clap default "120"); a garbage explicit value → 0 (JS NaN*1000 → NaN,
/// but the loop's `< timeoutMs` is then never true → immediate timeout; we clamp
/// to 0 which has the same effect).
fn timeout_ms(raw: &str) -> i64 {
    raw.trim().parse::<i64>().unwrap_or(0).saturating_mul(1000)
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
    run_send_pty_resolved(&session, message, wait, raw, full, timeout, false)
}

/// Unified-send entry into the existing PTY implementation. The caller has
/// already resolved and identity-fenced the target; accepting the `Session`
/// directly prevents a second name/prefix resolution from selecting a different
/// row between carrier selection and delivery. Explicit `send:pty` continues to
/// enter through [`run_send_pty`] with its unchanged flags and defaults.
///
/// `strict=true` maps known-failed or unaccepted-stuck submission to non-zero
/// (QS-3). Explicit `send:pty` uses `strict=false` for backward compatibility.
pub(super) fn run_send_pty_unified(session: &dispatch::model::Session, message: &str) -> i32 {
    run_send_pty_resolved(session, message, false, false, false, "120", true)
}

/// Shared resolved-target PTY body. Keeping the original explicit verb's body
/// here makes unified send reuse its write acknowledgements, integrity checks,
/// event semantics, and honest output without inventing a second PTY path.
///
/// `strict`: when true, known-failed submission (idle `!accepted`, busy
/// `stuck`) exits non-zero instead of warning+exit-0. Unified send sets this;
/// explicit `send:pty` keeps `false` for behavioral compatibility.
fn run_send_pty_resolved(
    session: &dispatch::model::Session,
    message: &str,
    wait: bool,
    raw: bool,
    full: bool,
    timeout: &str,
    strict: bool,
) -> i32 {
    // codex P1, R1 (codex-p1-spec section 2.3): refuse an unknown provider LOUDLY.
    //
    // codex-interactive: scoped to rows with no resolvable hosting (the
    // `attach_resolved` / `kill` narrowing). This body is provider-generic — it
    // writes to a pane through the mux — and a pane-hosted codex row is routed
    // here ON PURPOSE by the unified selector. The allow-list this helper carries
    // predates codex having a pane, so an unconditional call would refuse the very
    // session the selector just decided this path serves.
    if dispatch::provider::row_hosting(&session.provider, session.hosting.as_deref()).is_none() {
        if let Some(code) = common::refuse_unknown_provider("send:pty", session) {
            return code;
        }
    }

    // cold → dead (send.ts:105-108); no zmxName → not-in-zmx (send.ts:110-113).
    if session.status == SessionStatus::Cold {
        eprintln!("Session is dead. Use 'qd resume' first.");
        return 1;
    }
    let Some(zmx_name) = session.zmx_name.clone() else {
        // WRONG-LAYER FIX (C1 redfix): this condition is "the session has no live
        // mux session linked" — generic, NOT zmx-specific. Under the embedded
        // backend the stale "not in zmx" wording named the wrong layer. Key the
        // text off the selected backend: zmx keeps the BYTE-STABLE legacy wording;
        // embedded names the actual backend (qrmux). A bogus QD_MUX here would have
        // already failed `all_sessions` above, so the parse is effectively infallible
        // on this path; on the impossible error we fall back to the generic wording.
        match common::send_backend_label() {
            SendBackend::Embedded => {
                eprintln!(
                    "Session has no live qrmux session — cannot send (it may still be \
                     starting up; retry in a moment)."
                );
            }
            SendBackend::Zmx => {
                eprintln!("Session is not in zmx — cannot send.");
            }
        }
        return 1;
    };

    // M3 DEFENSIVE REFUSAL of an ATTENDED zmx target (BUILD-DIRECTIVES §1 ruling
    // (2), SUPERSEDING the plan's "zmx keeps legacy" for the attended case). The zmx
    // backend cannot host the polite machinery (journal/lock/countdown), so a blind
    // primary CR into an attended pane could clobber a human's in-progress draft.
    // Attendance is OBSERVED at the protocol seam — `zmx list`'s `clients=N` →
    // `session.zmx_clients` (the same signal reconcile.rs uses for "attached ⇒ never
    // touch") — never guessed. Sound ONLY for zmx: the embedded backend synthesizes
    // `clients = 0` (it observes attendance internally in the mux and defers there),
    // so gating on `SendBackend::Zmx` is load-bearing. The unattended zmx path
    // (clients == 0) keeps today's behavior + honest events untouched below.
    if dispatch::sendpty::refuse_attended_zmx(
        matches!(common::send_backend_label(), SendBackend::Zmx),
        session.zmx_clients,
    ) {
        // M5/T2 — fail CLOSED on an unknown count, with an HONEST message that never
        // asserts an attach we did not observe (the unknown arm says the count is
        // unreadable and attendance cannot be ruled out).
        eprintln!(
            "{}",
            dispatch::sendpty::attended_zmx_refusal_message(
                session.name.as_deref().unwrap_or(&session.session_id),
                session.zmx_clients,
            )
        );
        return 1;
    }

    let action = decide_send_pty(session.status.as_str());

    // L9a paths from HOME (for the jsonl projects dir + pid file dir).
    let env = RealEnv;
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // --wait preconditions FIRST (parity, send.ts:116-123): resolve the jsonl
    // path + record the size BEFORE sending, so the anchor loop only sees records
    // written by/after our message. W8 (wart-wave-spec §4): a CHUNKED payload
    // resolves + snapshots too — the verify-after-submit read-back needs the
    // pre-delivery offset. --wait keeps its hard requirement (loud exit 1);
    // verify-only resolution failure DEGRADES (warned at the verify step — the
    // send itself must not gain a new precondition).
    let needs_verify = dispatch::submit::payload_needs_verify(message);
    // ACK-2 D10 (R6/R7): the transcript-offset snapshot is now
    // UNCONDITIONAL-WHEN-RESOLVABLE — we ALWAYS try to resolve the jsonl path +
    // stat its length so the `send-initiated` event carries the recovery-read
    // window key, even on a bare (no --wait, single-chunk) send. The verb's OWN
    // preconditions are unchanged: only `--wait` still loudly requires the
    // transcript (exit 1 below); a resolution failure remains non-fatal for the
    // event (the fields are simply absent) and for the verify path (it degrades).
    // codex P1 W7 (codex-p1-spec section 7.2): the transcript-location fallback
    // dispatches through the provider seam, keyed on this row's provider value.
    // send:pty's W1 refusal (refuse_unknown_provider, above) already gated an
    // unknown provider, so the value here is claude-code or the parked opencode;
    // `provider_for` resolves claude-code and degrades opencode (parked, not a
    // Provider impl) to the claude derivation for path resolution (same
    // render-survival posture as the gather). The resolved path is byte-identical
    // to the old `find_jsonl_path` call (ClaudeProvider::transcript_path delegates).
    let provider = dispatch::provider::provider_for(&session.provider)
        .unwrap_or_else(|| dispatch::provider::provider_for("claude-code").unwrap());
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
            let fx = dispatch::provider::ProviderFx {
                await_relay: None,
                env: &env,
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
                &dispatch::provider::SessionKey {
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
        eprintln!("Cannot find conversation JSONL file.");
        return 1;
    }

    // Op dir = session.socketDir ?? canonical (Bug D — every write AND every
    // screen read uses this SAME dir so they hit the SAME zmx session,
    // send.ts:130-133).
    let canonical = resolve_zmx_dir(&env);
    let op_dir: PathBuf = session
        .socket_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(canonical);

    // Backend-selected mux (C1 D3). A live session carries its socket_dir (tagged
    // by the backend's list), so the embedded lane writes to the qrmux dir.
    let mux_box = match common::real_mux() {
        Ok(m) => m,
        Err(code) => return code,
    };
    let mux: &dyn Mux = mux_box.as_ref();
    let clock = RealClock;
    let sleeper = RealSleeper;

    let label = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.clone());

    // A6 §4.1: capture identity for the content-free invoked line emitted on a
    // SUCCESSFUL send (any exit-0 path below). Best-effort — see `invoked_send`.
    let invoked_sid = session.session_id.clone();
    let invoked_name = session.name.clone();

    // --- ACK-2 §9: engine event emission (additive, best-effort non-fatal) ----
    // The sessionId is ALWAYS resolved on the send:pty path, so the events file
    // is keyed on it (§4.1; no byname fallback here). The state_dir honors QD_HOME
    // (§4.1 / ADD-14): resolve it via the QD_HOME-aware paths.
    let ev_state = dispatch::paths::QdPaths::from_home_env(&paths.home, &env).state_dir;
    let writer = EventWriter::for_key(
        &ev_state,
        &session.session_id,
        Some(session.session_id.clone()),
        session.name.clone(),
    );
    let send_id = events::mint_send_id(&clock);

    // §2.3.1 send-initiated: minted BEFORE any chunk write. `send_path` keys off
    // the decided action (busy → queued; idle → idle). The per-chunk shas come
    // from the PRODUCTION splitter (chunk_text(msg,1024)); the count is the same
    // splitter's length. transcript/offset present when resolvable (D10).
    let chunks_vec = dispatch::submit::chunk_text(message, dispatch::events::CHUNK_BYTES);
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
        &clock,
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
            content_preview: Some(dispatch::redact::redact_for_preview(
                message,
                dispatch::events::PREVIEW_CAP_BYTES,
            )),
        },
    );

    // ===================== M3: EMBEDDED handoff path =======================
    // The embedded (qrmux) backend HANDS the send to the mux DELIVERY SURFACE
    // (v5 PendingDelivery) instead of orchestrating raw writes. qd emitted ONLY the
    // pre-handoff `send-initiated` (above); from the `DeliveryQueued` receipt onward
    // the MUX owns the send's lifecycle and emits EXACTLY ONE terminal per send_id
    // to the authoritative ledger (the single-writer split — qd writes NO terminal
    // on this path). The ZMX backend falls through to today's raw-write orchestration
    // (+ honest events); the attended-zmx refusal above already protected it.
    if matches!(common::send_backend_label(), SendBackend::Embedded) {
        let args = dispatch::embedded_mux::PendingDeliveryArgs {
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
        match dispatch::embedded_mux::embedded_pending_delivery(&op_dir, args) {
            Ok(acked) => debug_assert_eq!(acked, send_id, "mux echoes the qd-minted send_id"),
            Err(e) => {
                // DOOR failure BEFORE the mux accepted: nothing spooled, no dangling
                // send. Loud synchronous exit, NO qd-minted terminal (matching today's
                // door discipline — the standing send-initiated + this loud exit is the
                // C1 account; any post-acceptance terminal is the mux's, never qd's).
                eprintln!("Could not hand \"{label}\" its send to the mux delivery surface: {e}");
                return 1;
            }
        }

        if !wait {
            // The mux resolves to exactly one terminal AFTER we exit — drop-immune,
            // no reader-presence dependency (invariant 3). HONEST: accepted for
            // delivery, resolves asynchronously; NEVER claims "landed" (QS-6).
            //
            // F2 CONTRACT (W8 truncation, explicit — not a silent drop): pre-M3, a
            // chunked no-`--wait` send ran qd's SYNCHRONOUS W8 read-back and exited 1
            // on truncation. Under the single-writer async handoff qd has NO terminal
            // in hand at exit, so there is deliberately NO synchronous truncation
            // verdict here: the no-`--wait` send is honestly `queued`, and the mux's
            // LandingProbe surfaces a truncated landing as a `turn-anchored-mismatch`
            // terminal on the ledger (P6 honest-events). That mismatch IS observed by
            // `--wait` (mapped to an honest non-zero exit — see the mismatch arm below,
            // proven by `m3_embedded_delivery_e2e::…_detects_truncation_as_mismatch`)
            // and by the on-restart reconcile (M5(c)); it is NOT observable in the
            // no-`--wait` sender's exit code (the sender is gone). Re-entry for a
            // no-`--wait` truncation signal: M5(c) reconcile.
            println!("Message queued to \"{label}\"");
            invoked_send(&invoked_sid, invoked_name.as_deref());
            return 0;
        }

        // --wait: watch the LEDGER to a REAL 7-set terminal (NEVER return on the
        // non-terminal DeliveryQueued ack — a queued ack is NOT delivery). READ-ONLY:
        // qd emits nothing.
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
        let bound_ms = timeout_ms(timeout).max(EMBEDDED_WAIT_FLOOR_MS);
        let watch_deps = RealTerminalWatchDeps {
            state_dir: ev_state.clone(),
            session_id: session.session_id.clone(),
            name: session.name.clone(),
            clock: &clock,
            sleeper: &sleeper,
        };
        eprint!("Waiting for delivery");
        {
            use std::io::Write;
            let _ = std::io::stderr().flush();
        }
        match dispatch::sendpty::watch_terminal(&watch_deps, &send_id, bound_ms, 500) {
            // F5 (shared success-terminal identity): "which terminal means
            // delivered" is now the leaf crate's ONE definition
            // (`is_success_terminal`), consumed here AND by the mux banner
            // (qrmux/attended/driver.rs `toast_kind_for`) — no locally-minted
            // "message-seen" success literal on either side (M4 F5/M2 fold).
            Some(term) if events::is_success_terminal(&term.event) => {
                eprintln!(" delivered");
                // QS-7: preserve the user-facing --wait reply print via the EXISTING
                // JSONL anchor loop — WITHOUT emitting any terminal (the mux already
                // emitted the one terminal; single-writer) and WITHOUT the WatchGuard
                // or the status-transition emitter (`status_emit: None`): qd writes
                // NOTHING to the ledger on this path.
                let Some(jp) = jsonl_path.clone() else {
                    // Delivery is confirmed (message-seen); we just can't read the
                    // reply back without a transcript. Report the confirmed success.
                    println!(
                        "Message delivered to \"{label}\" (reply not captured: transcript unavailable)"
                    );
                    invoked_send(&invoked_sid, invoked_name.as_deref());
                    return 0;
                };
                let pid_file = session
                    .pid
                    .filter(|&p| p != 0)
                    .map(|pid| paths.sessions_dir.join(format!("{pid}.json")));
                let deps = RealWaitDeps {
                    jsonl_path: jp.clone(),
                    start_offset,
                    pid_file,
                    clock: &clock,
                    sleeper: &sleeper,
                    status_emit: None,
                };
                let outcome = run_wait_loop(&deps, message, timeout_ms(timeout), 500);
                let mode = if raw {
                    ExtractMode::Raw
                } else if full {
                    ExtractMode::Full
                } else {
                    ExtractMode::Default
                };
                invoked_send(&invoked_sid, invoked_name.as_deref());
                return match outcome {
                    WaitOutcome::Complete { lines, anchor } => {
                        eprintln!(" done");
                        match dispatch::sendpty::capture_or_defect(&lines, anchor, mode) {
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
                    WaitOutcome::Died => {
                        eprintln!(" session died");
                        eprintln!("Delivered to \"{label}\", but the session exited before its reply completed.");
                        1
                    }
                    WaitOutcome::TimedOut { .. } => {
                        eprintln!(" timeout");
                        eprintln!("Delivered to \"{label}\", but timed out waiting for the reply.");
                        1
                    }
                    WaitOutcome::SourceError(reason) => {
                        eprintln!(" error");
                        eprintln!(
                            "Delivered to \"{label}\", but the transcript integrity was lost while \
                             reading the reply: {reason}"
                        );
                        1
                    }
                };
            }
            Some(term) => {
                // A mux failure/mismatch terminal (send-failed / seen-failed /
                // turn-anchored-mismatch / anchor-timeout / pending-abandoned). Honest
                // failure, named from the ledger; no reply collection.
                eprintln!(" failed");
                eprintln!(
                    "Delivery to \"{label}\" did not land ({}) — check: qd attach {label}",
                    term.event
                );
                return 1;
            }
            None => {
                // Bound elapsed with NO terminal: the mux left the send Pending (e.g. a
                // busy-queued turn landing past the mux's landing window) or it is still
                // counting down under continuous typing. HONEST still-pending — never a
                // false "landed", never a false failure (invariant 1). The send is
                // spooled in the mux and resolves later (reconcile / a later landing).
                eprintln!(" pending");
                eprintln!(
                    "Delivery to \"{label}\" is still pending (accepted by the mux, not yet \
                     confirmed landed) — it will resolve in the mux; check: qd attach {label}"
                );
                return 1;
            }
        }
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
            let acks = send_text_chunked_mux(mux, &sleeper, &op_dir, &zmx_name, message);
            sleeper.sleep_ms(dispatch::submit::TWO_WRITE_SETTLE_MS);
            let _ = mux.send(&op_dir, &zmx_name, "\r");

            // §2.3.2 chunks-delivered: emitted only when EVERY text chunk's
            // mux.send returned Ok (a partial leaves the send dangling by design).
            if acks.all_ok() {
                events::warn_emit(
                    &writer,
                    &clock,
                    &Payload::ChunksDelivered {
                        send_id: send_id.clone(),
                        chunks_acked: acks.acked,
                        ack_source: ack_source_label().to_string(),
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
                return write_failed_exit(acks);
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
                sleeper.sleep_ms(dispatch::submit::TWO_WRITE_SETTLE_MS);
                stuck = composer_holds_message(&read_screen(mux, &op_dir, &zmx_name), message);
            }
            if stuck_was_observed && !stuck {
                events::warn_emit(
                    &writer,
                    &clock,
                    &Payload::ComposerCleared {
                        send_id: send_id.clone(),
                    },
                );
            }

            if !wait {
                if stuck {
                    // TS WARNING wording verbatim (send.ts:172-176).
                    eprintln!(
                        "WARNING: message may be stuck unsubmitted in \"{label}\"'s composer \
                         — check with: qd attach {label}"
                    );
                    // QS-3 (unified send): known-stuck submission is a non-zero
                    // result for the primary verb (strict mode only; send:pty
                    // preserves its existing warning+continue contract).
                    if strict {
                        return 1;
                    }
                }
                println!("Message queued in \"{label}\" (session busy)");

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
                        // snapshotted at send.rs ~309 BEFORE typing), which sits PAST
                        // the prior in-flight turn's user record → the scan can never
                        // anchor on it (even an identical body), only on THIS turn.
                        let bqdeps = DeferredBusyQueuedDeps {
                            jsonl_path: jp.clone(),
                            pre_send_offset: start_offset,
                            clock: &clock,
                            sleeper: &sleeper,
                        };
                        match dispatch::submit::deferred_verify_busy_queued(
                            &bqdeps,
                            message,
                            dispatch::submit::BUSY_QUEUED_VERIFY_TIMEOUT_S,
                            dispatch::submit::VERIFY_POLL_MS,
                        ) {
                            dispatch::submit::PayloadVerifyOutcome::Verified => {
                                // The queued turn landed uniquely past the floor →
                                // the on-received `message-seen` fires (the consumer's
                                // reason=Seen gate opens the next link). Joined by
                                // send_id (transport-agnostic) — same emit the idle
                                // and fresh-child resolvable paths use.
                                emit_w8_message_seen(&writer, &clock, &send_id, message);
                            }
                            _ => {
                                // Miss / ≥2 identical bodies past the floor
                                // (Unattributable) / transcript unresolvable
                                // (SourceUnavailable) → PENDING, never a wrong-fire
                                // (anti-phantom, constraint 3). The promise stays
                                // visibly PENDING — symmetric with the idle NoRecord
                                // no-wait degrade.
                                eprintln!(
                                    "WARNING: could not verify the queued payload landed in \
                                     \"{label}\"'s transcript within {}s (the promise stays \
                                     PENDING) — check: qd attach {label}",
                                    dispatch::submit::BUSY_QUEUED_VERIFY_TIMEOUT_S
                                );
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
                        eprintln!(
                            "WARNING: could not resolve \"{label}\"'s transcript to verify the \
                             queued payload (the promise stays PENDING) — check: qd attach {label}"
                        );
                    }
                }

                invoked_send(&invoked_sid, invoked_name.as_deref());
                return 0;
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
                // UNTOUCHED — this is a bin-local impl swap only.
                let pid_file = paths.sessions_dir.join(format!("{pid}.json"));
                let rec_deps = RecordingIdleDeliverDeps {
                    mux,
                    clock: &clock,
                    sleeper: &sleeper,
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
                        eprintln!(
                            "WARNING: Message sent to \"{label}\" but session did not go busy \
                             — it may be stuck unsubmitted in the composer."
                        );
                        // QS-3 strict exit deferred: emitted AFTER chunks-delivered so the
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
                let acks = send_text_chunked_mux(mux, &sleeper, &op_dir, &zmx_name, message);
                sleeper.sleep_ms(dispatch::submit::TWO_WRITE_SETTLE_MS);
                let _ = mux.send(&op_dir, &zmx_name, "\r");
                idle_acks = acks;
            }

            // §2.3.2 chunks-delivered: all text chunks acked → emit (partial leaves
            // the send dangling by design; verb behavior unchanged).
            if idle_acks.all_ok() {
                events::warn_emit(
                    &writer,
                    &clock,
                    &Payload::ChunksDelivered {
                        send_id: send_id.clone(),
                        chunks_acked: idle_acks.acked,
                        ack_source: ack_source_label().to_string(),
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
                return write_failed_exit(idle_acks);
            }

            // QS-3 strict exit (idle lane, deferred from the not-accepted check above):
            // chunks-delivered has now been emitted (or write_failed_exit returned), so the
            // event stream is complete before we exit. Strict mode for unified send only;
            // send:pty keeps its existing warning+exit-0 contract via strict=false.
            if not_accepted && !wait && strict {
                return 1;
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
                            clock: &clock,
                            sleeper: &sleeper,
                        };
                        match dispatch::submit::verify_chunked_payload(
                            &vdeps,
                            message,
                            dispatch::submit::VERIFY_TIMEOUT_S,
                            dispatch::submit::VERIFY_POLL_MS,
                        ) {
                            dispatch::submit::PayloadVerifyOutcome::Verified => {
                                if wait {
                                    // §9 --wait idle arm (MF1): KEEP `turn-anchored`.
                                    // The W8 verify emits THE landed signal and
                                    // verify_anchored suppresses the duplicate Complete
                                    // turn-anchored (m-1). recovered/attribution absent
                                    // (a LIVE anchor); line_index 0 (the verify helper
                                    // returns texts, not indices). UNCHANGED from today.
                                    emit_w8_anchored(
                                        &writer,
                                        &clock,
                                        &send_id,
                                        message,
                                        jp,
                                        start_offset,
                                    );
                                    verify_anchored = true;
                                } else {
                                    // §X.3.4/§X.5 — async NO-WAIT W8 success → the
                                    // on-received `message-seen` (NOT turn-anchored, so
                                    // a consumer's reason=Seen gate can never be tripped by a
                                    // W8/--wait anchor). The call-base goal turn
                                    // (runner.rs:250, send_pty(.., false)) IS this path
                                    // → the consumer routes it to Fired{reason=Seen}.
                                    emit_w8_message_seen(&writer, &clock, &send_id, message);
                                }
                            }
                            dispatch::submit::PayloadVerifyOutcome::Truncated {
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
                                        &clock,
                                        &send_id,
                                        message,
                                        jp,
                                        start_offset,
                                        expected,
                                        recorded,
                                    );
                                    eprintln!(
                                        "ERROR: payload truncated in delivery to \"{label}\": expected \
                                         {expected} bytes, recorded {recorded}.\n  The message submitted — \
                                         do NOT blindly resend (double-submit risk).\n  Attach: qd attach {label}"
                                    );
                                    return 1;
                                }
                                // MF2 — a NEWLY-UNGATED ≤1-chunk no-wait send must NOT
                                // newly fail-loud (it exited 0 before the ungate).
                                // Degrade to a warn; emit NO terminal (a partial
                                // read-back of a short send is not a delivery failure —
                                // the promise stays PENDING, §X.6).
                                eprintln!(
                                    "WARNING: could not fully verify the short payload to \"{label}\" \
                                     (read-back truncated: expected {expected}, recorded {recorded}) — \
                                     check: qd attach {label}"
                                );
                            }
                            dispatch::submit::PayloadVerifyOutcome::NoRecord => {
                                if needs_verify {
                                    // >1-chunk — the strict exit-1 failure, UNCHANGED.
                                    eprintln!(
                                        "ERROR: could not verify payload arrival in \"{label}\"'s \
                                         transcript within {}s (no user record appeared).\n  \
                                         Attach: qd attach {label}",
                                        dispatch::submit::VERIFY_TIMEOUT_S
                                    );
                                    return 1;
                                }
                                // MF2 — ≤1-chunk no-wait: degrade-warn, NOT exit-1. No
                                // record within the W8 window ⇒ not-yet-seen ⇒ the
                                // promise stays PENDING (latency is normal, §X.6). A
                                // later landing is unobserved on the pty path (bounded
                                // W8) — pty's accepted reliability (§X.6).
                                eprintln!(
                                    "WARNING: could not verify short-payload arrival in \"{label}\"'s \
                                     transcript within {}s — check: qd attach {label}",
                                    dispatch::submit::VERIFY_TIMEOUT_S
                                );
                            }
                            dispatch::submit::PayloadVerifyOutcome::Unattributable => {
                                eprintln!(
                                    "WARNING: could not attribute the delivered payload in \
                                     \"{label}\"'s transcript — check: qd attach {label}"
                                );
                            }
                            dispatch::submit::PayloadVerifyOutcome::SourceUnavailable(why) => {
                                eprintln!(
                                    "WARNING: could not verify payload delivery to \"{label}\" \
                                     ({why}) — check: qd attach {label}"
                                );
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
                        // (the `--wait` hard precondition already exited 1 above
                        // when the path was unresolvable), so this is the no-wait
                        // on-received path only; on miss it degrades to a VISIBLE
                        // PENDING stall, never a wrong-fire (symmetric with the
                        // NoRecord no-wait degrade).
                        let ddeps = DeferredFreshChildDeps {
                            projects_dir: paths.projects_dir.clone(),
                            session_id: session.session_id.clone(),
                            clock: &clock,
                            sleeper: &sleeper,
                        };
                        match dispatch::submit::deferred_verify_fresh_child(
                            &ddeps,
                            message,
                            dispatch::submit::VERIFY_TIMEOUT_S,
                            dispatch::submit::VERIFY_POLL_MS,
                        ) {
                            dispatch::submit::PayloadVerifyOutcome::Verified => {
                                // The fresh child's no-wait pty `message-seen`
                                // fires (the S1 stall is closed); the consumer's OnReceived
                                // gate (joining by send_id) fires link N+1.
                                emit_w8_message_seen(&writer, &clock, &send_id, message);
                            }
                            _ => {
                                eprintln!(
                                    "WARNING: could not verify payload delivery to \"{label}\" \
                                     (fresh-child transcript did not resolve/anchor within the \
                                     deferred window — the promise stays PENDING) — check: \
                                     qd attach {label}"
                                );
                            }
                        }
                    }
                }
            }

            if !wait {
                println!("Message sent to {label}");
                invoked_send(&invoked_sid, invoked_name.as_deref());
                return 0;
            }
        }
    }

    // --- --wait JSONL anchor loop (both paths converge, send.ts:208-265) -----
    // pid file for the status read inside the loop.
    let pid_file = session
        .pid
        .filter(|&p| p != 0)
        .map(|pid| paths.sessions_dir.join(format!("{pid}.json")));
    let jp = jsonl_path.expect("jsonl_path set whenever wait is true");

    eprint!("Waiting for response");
    use std::io::Write;
    let _ = std::io::stderr().flush();

    // §9 status-transition seam (red-team R4): the emission lives in the DEPS
    // IMPL, never run_wait_loop's pure core. RealWaitDeps::read_status wraps
    // read_pid_status with a last-status Cell + an emitter; the pure core +
    // WaitDeps trait stay intact. The emitter borrows the writer/clock; the loop
    // is the only caller, so a RefCell-free Cell suffices.
    let last_status: Cell<Option<String>> = Cell::new(None);
    let deps = RealWaitDeps {
        jsonl_path: jp.clone(),
        start_offset,
        pid_file,
        clock: &clock,
        sleeper: &sleeper,
        status_emit: Some(StatusEmit {
            writer: &writer,
            clock: &clock,
            last: &last_status,
        }),
    };

    // §9 / rev C row 24 WatchGuard: armed for the duration of the --wait watch as
    // a panic/early-return safety net — an unwind that skips the disarm below Drops
    // it → pending-abandoned{watch-interrupted}. Disarmed UNCONDITIONALLY once the
    // wait resolves (post amend rider 3, the Died / TimedOut / SourceError arms ALL
    // resolve to NO terminal — the send is left dead-dangling-recoverable, so the
    // disarm is what stops Drop from re-minting the foreclosing terminal on those
    // paths). SIGKILL bypasses Drop and is covered by the reader-side dead-writer
    // rule (§7), same as those non-foreclosing paths now are.
    let guard = WatchGuard::arm(&writer, &clock, &send_id);
    let outcome = run_wait_loop(&deps, message, timeout_ms(timeout), 500);

    let mode = if raw {
        ExtractMode::Raw
    } else if full {
        ExtractMode::Full
    } else {
        ExtractMode::Default
    };

    let code = match outcome {
        WaitOutcome::Died => {
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
            // `guard.disarm()` below keeps this path from re-minting the terminal via Drop.
            eprintln!(" session died");
            eprintln!("Session exited while waiting for response.");
            1
        }
        WaitOutcome::TimedOut { anchored } => {
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
            // `guard.disarm()` below keeps Drop from re-minting the terminal.
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
        WaitOutcome::Complete { lines, anchor } => {
            // §9: --wait Complete → turn-anchored (terminal). The anchor index is
            // the producer's own (sticky); start_offset is the pre-send snapshot.
            // m-1: SKIPPED when the W8 verify already anchored this send — one
            // landed signal per send_id, never a systematic duplicate.
            if !verify_anchored {
                events::warn_emit(
                    &writer,
                    &clock,
                    &Payload::TurnAnchored {
                        send_id: send_id.clone(),
                        content_sha256: events::sha256_hex(message.as_bytes()),
                        anchor: Anchor {
                            transcript: jp.display().to_string(),
                            start_offset,
                            line_index: anchor.unwrap_or(0) as u64,
                        },
                        recovered: false,
                        attribution: None,
                    },
                );
            }
            eprintln!(" done");
            // The SEND landed (anchored) — the A6 invoked line keys on the send,
            // not the capture, so it stays unconditional on this arm even when
            // the capture below fails loud (declared judgment call, B2 item 3).
            invoked_send(&invoked_sid, invoked_name.as_deref());
            // B2 item 3 — the binding capture invariant: --wait NEVER returns
            // an empty capture as success. capture_or_defect Ok = real content
            // (exit 0); Err = a LOUD non-zero exit naming what was observed.
            // SANCTIONED DIVERGENCE from TS (which printed "(no text
            // response)" / nothing and exited 0 — incl. the old empty --raw
            // silent-0 special case).
            match dispatch::sendpty::capture_or_defect(&lines, anchor, mode) {
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
        WaitOutcome::SourceError(reason) => {
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
            // deferred to C6); the unconditional `guard.disarm()` below keeps this
            // path from re-minting the terminal via Drop.
            // ADD-8 W5: integrity loss on the JSONL source — loud, never a
            // silent whole-file re-anchor. Rust-only guard (TS has the silent
            // fallback class; named in the A4 matrix).
            eprintln!(" error");
            eprintln!("Conversation JSONL integrity lost while waiting: {reason}");
            1
        }
    };
    // Post amend rider 3: the Died / TimedOut / SourceError arms emit NO terminal —
    // they leave the send dead-dangling-recoverable (findings G/B), and only the
    // Complete arm mints a (success) turn-anchored. Disarm UNCONDITIONALLY so
    // WatchGuard's Drop never re-mints a foreclosing pending-abandoned{watch-
    // interrupted} on the non-foreclosing paths (that is exactly why B kept it).
    // SIGKILL bypasses Drop and is covered by the reader-side dead-writer rule (§7).
    guard.disarm();
    code
}

/// §X.3.4 (3-phase delivery, on-received) — the async NO-WAIT W8 success signal.
/// A Verified read-back on the async no-wait path IS the recipient pulling the
/// message into working context. Deliberately `message-seen` (a NEW terminal kind,
/// NOT `turn-anchored`) so the consumer's on-received reason=Seen gate is satisfied and no
/// `--wait`/W8 `turn-anchored` anchor can ever false-fire it (§X.5).
/// `content_sha256 = sha256(message)` — the robust pty key (the same bytes the
/// sender hashed). Carries no prose (§X.7).
fn emit_w8_message_seen(writer: &EventWriter, clock: &dyn Clock, send_id: &str, message: &str) {
    events::warn_emit(
        writer,
        clock,
        &Payload::MessageSeen {
            send_id: send_id.to_string(),
            content_sha256: events::sha256_hex(message.as_bytes()),
        },
    );
}

/// §9 W8 anchored emission (idle path Verified): the live verify read-back IS the
/// landed signal. `recovered` false, `attribution` absent; the anchor uses the
/// verify-window `offset` and `line_index` 0 (the verify helper returns texts, not
/// indices — documented unknown).
fn emit_w8_anchored(
    writer: &EventWriter,
    clock: &dyn Clock,
    send_id: &str,
    message: &str,
    transcript: &Path,
    offset: u64,
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
                line_index: 0,
            },
            recovered: false,
            attribution: None,
        },
    );
}

/// §2.3.4 / §9 W8 anchored-mismatch emission (idle path Truncated). The
/// PayloadVerifyOutcome carries lengths only; we re-read the user texts past the
/// offset HERE and sha the longest truncation-signature record for `actual_sha`
/// (the honest value). On a re-read failure `actual_sha` is "" (named gap), never
/// invented. `expected_sha` = sha256(message).
#[allow(clippy::too_many_arguments)]
fn emit_w8_mismatch(
    writer: &EventWriter,
    clock: &dyn Clock,
    send_id: &str,
    message: &str,
    transcript: &Path,
    offset: u64,
    expected: usize,
    recorded: usize,
) {
    let actual_sha = dispatch::submit::read_user_texts_past_offset(transcript, offset)
        .ok()
        .and_then(|texts| {
            dispatch::submit::longest_truncation_signature(&texts, message)
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

/// `zmx history <name>` pinned to `op_dir` — the composer screen for the content-
/// verified CR (send.ts readScreen). Errors yield an empty screen (a missing
/// screen can't hold our message → not stuck → no CR; fail-safe).
fn read_screen(mux: &dyn Mux, op_dir: &Path, zmx_name: &str) -> String {
    mux.history(op_dir, zmx_name).unwrap_or_default()
}

/// The real (fs/clock-backed) [`WaitDeps`] for the send:pty `--wait` loop.
/// W8 [`dispatch::submit::VerifyDeps`] for the send:pty idle path: the transcript was
/// resolved + offset-snapshotted PRE-send, so each poll is a direct read past
/// that offset (the shared `read_user_texts_past_offset` helper — same
/// shrink/boundary loud-degrade semantics as the --wait loop's reader).
struct SendPtyVerifyDeps<'a> {
    jsonl_path: PathBuf,
    offset: u64,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl dispatch::submit::VerifyDeps for SendPtyVerifyDeps<'_> {
    fn read_user_texts(&self) -> Result<Vec<String>, String> {
        dispatch::submit::read_user_texts_past_offset(&self.jsonl_path, self.offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// W8 [`dispatch::submit::DeferredVerifyDeps`] for the FRESH-CHILD no-wait pty path
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

impl dispatch::submit::DeferredVerifyDeps for DeferredFreshChildDeps<'_> {
    fn resolve_path(&self) -> Option<PathBuf> {
        dispatch::jsonl::find_jsonl_path(&self.projects_dir, &self.session_id, None)
    }
    fn first_user_offset(&self, path: &Path) -> Option<u64> {
        dispatch::submit::first_user_text_offset(path)
    }
    fn read_user_texts(&self, path: &Path, offset: u64) -> Result<Vec<String>, String> {
        dispatch::submit::read_user_texts_past_offset(path, offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// W8 [`dispatch::submit::DeferredVerifyDeps`] for the §7-B BUSY-QUEUED no-wait pty
/// path (PTY-FIX-DESIGN.md §4) — the busy-queued analogue of
/// [`DeferredFreshChildDeps`]. The recipient is an EXISTING session whose
/// transcript was already resolved pre-send (`resolve_path` just hands it back; no
/// poll-into-existence), and the high-water floor is the CAPTURED PRE-SEND OFFSET
/// (`pre_send_offset` = the idle path's `start_offset`, snapshotted before typing,
/// PAST the prior in-flight turn's user record — constraint 1), NOT the first
/// user-text record. `read_user_texts` reuses `read_user_texts_past_offset`
/// verbatim, so the uniqueness/high-water/PENDING policy is shared byte-for-byte
/// with the fresh-child path via [`dispatch::submit::deferred_verify_busy_queued`].
struct DeferredBusyQueuedDeps<'a> {
    jsonl_path: PathBuf,
    pre_send_offset: u64,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl dispatch::submit::DeferredVerifyDeps for DeferredBusyQueuedDeps<'_> {
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
        dispatch::submit::read_user_texts_past_offset(path, offset)
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
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
            events::warn_emit(
                self.writer,
                self.clock,
                &Payload::StatusTransition {
                    status: status.to_string(),
                    source: "status-file-poll".to_string(),
                },
            );
        }
    }
}

/// Real deps for the M3 embedded `--wait` terminal watcher
/// ([`dispatch::sendpty::watch_terminal`]): read the TARGET session's merged
/// delivery ledger (read-only — the mux owns the terminal), sleep, and the real
/// clock. Emits NOTHING (single-writer split: qd only READS the mux's terminal).
struct RealTerminalWatchDeps<'a> {
    state_dir: PathBuf,
    session_id: String,
    name: Option<String>,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
}

impl dispatch::sendpty::TerminalWatchDeps for RealTerminalWatchDeps<'_> {
    fn read_records(&self) -> Vec<dispatch::events::EventRecord> {
        // The SAME key derivation qd's `EventWriter::for_key` and the mux's
        // `resolve_state_dir`/`ledger_path` use → the one authoritative
        // `<state_dir>/sessions/<sessionId>.events.jsonl` the mux wrote its terminal to.
        dispatch::events::read_merged(&self.state_dir, Some(&self.session_id), self.name.as_deref())
            .records
    }
    fn sleep(&self, ms: u64) {
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
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
    /// §9 status-transition emitter (None in the pure unit tests of the loop).
    status_emit: Option<StatusEmit<'a>>,
}

impl WaitDeps for RealWaitDeps<'_> {
    fn read_lines(&self) -> Result<Vec<dispatch::sendpty::ParsedLine>, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::exec::ExecResult;
    use dispatch::mux::MuxSession;
    use std::io;

    // A minimal mux whose `send` succeeds or fails on demand — the seam G3 needs
    // to prove chunks-delivered fires ONLY when every text chunk acked. Only
    // `send`/`history` carry meaning; the rest are unreachable on these paths.
    struct ProbeMux {
        send_ok: bool,
    }
    impl dispatch::mux::Mux for ProbeMux {
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
    impl dispatch::mux::Mux for PartialMux {
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

    #[test]
    fn ack_source_label_is_one_of_the_two_named_channels() {
        // §2.3.2: the channel name is honest — exactly input-sent | cli-exit.
        assert!(matches!(ack_source_label(), "input-sent" | "cli-exit"));
    }

    #[test]
    fn recording_idle_deps_count_only_text_not_cr() {
        // The idle recorder counts send_text chunks; send_cr (the "\r") is NOT a
        // chunks-delivered ack (only text chunks are evidence).
        let mux = ProbeMux { send_ok: true };
        let clock = RealClock;
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
        assert_eq!(write_failed_exit(acks), 11);
        // The EXACT stderr line (§5.1 wording, pinned by the contract).
        assert_eq!(
            write_failed_line(acks),
            "ERROR: PTY write failed (1/2 chunks acked) — see events file"
        );
    }

    #[test]
    fn add18_all_chunks_err_is_write_failed_exit_11() {
        // (b) every chunk errs → 0/2 acked → exit 11 + the exact line.
        let mux = ProbeMux { send_ok: false };
        let acks = send_text_chunked_mux(&mux, &NoSleep, Path::new("/d"), "wk", &two_chunk_msg());
        assert_eq!(acks.acked, 0);
        assert!(!acks.all_ok());
        assert_eq!(write_failed_exit(acks), 11);
        assert_eq!(
            write_failed_line(acks),
            "ERROR: PTY write failed (0/2 chunks acked) — see events file"
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

    // QS-3 strict-mode structural guard: `run_send_pty_resolved` MUST return 1 in
    // both the "stuck" (busy-lane, still in composer) and "not accepted" (idle-lane,
    // never went busy) paths when `strict=true`. The explicit `send:pty` verb passes
    // `strict=false` and preserves its warning+continue contract; unified `qd send`
    // passes `strict=true` so it cannot exit 0 on a known-failed submission.
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
        let src = include_str!("send.rs");
        let fn_start = src
            .find("fn run_send_pty_resolved(")
            .expect("run_send_pty_resolved must exist");
        let after_fn = &src[fn_start..];
        // Scope to the function body: emit_w8_message_seen immediately follows.
        let fn_end = after_fn
            .find("\nfn emit_w8_message_seen(")
            .expect("emit_w8_message_seen must follow run_send_pty_resolved");
        let body = &after_fn[..fn_end];

        // --- stuck path (busy lane) ---
        // Positive control: warning is present.
        assert!(
            body.contains("message may be stuck unsubmitted"),
            "stuck warning must still exist in run_send_pty_resolved"
        );
        // The stuck guard fires immediately after the warning (within the `if !wait` branch).
        let stuck_warn_pos = body.find("message may be stuck unsubmitted").unwrap();
        let stuck_region = &body[stuck_warn_pos..(stuck_warn_pos + 600).min(body.len())];
        assert!(
            stuck_region.contains("if strict {"),
            "run_send_pty_resolved must guard the stuck path with `if strict {{` (QS-3). \
             Region after warning:\n{stuck_region}"
        );
        assert!(
            stuck_region.contains("return 1;"),
            "run_send_pty_resolved must return 1 in the strict stuck path (QS-3). \
             Region after warning:\n{stuck_region}"
        );

        // --- not-accepted path (idle lane, deferred check) ---
        // Positive control: warning is present.
        assert!(
            body.contains("did not go busy"),
            "not-accepted warning must still exist in run_send_pty_resolved"
        );
        // The idle-lane strict exit is DEFERRED to after chunks-delivered for event
        // symmetry. `not_accepted` captures outcome.accepted==false from the pid-known
        // branch so it remains visible outside the if-let scope.
        // Pin the deferred compound check (the whole expression).
        assert!(
            body.contains("not_accepted && !wait && strict"),
            "run_send_pty_resolved must have the deferred idle-lane strict check \
             `not_accepted && !wait && strict` (QS-3, idle-lane guard after \
             chunks-delivered emit). Body does not contain the guard."
        );
        // Confirm return 1 follows it (within a tight window).
        let deferred_pos = body
            .find("not_accepted && !wait && strict")
            .unwrap();
        let deferred_region = &body[deferred_pos..(deferred_pos + 60).min(body.len())];
        assert!(
            deferred_region.contains("return 1;"),
            "the deferred idle-lane strict check must return 1 (QS-3). \
             Region:\n{deferred_region}"
        );
    }

    // QS-3 wiring guard (F-1 Fable finding): pins that `run_send_pty_unified` wires
    // the unified entry with `strict=true`. A one-token regression (`true`→`false`)
    // would silently disconnect QS-3 from `qd send` while all other tests stay green.
    // Structural form because the call site is the only wiring; no mock can exercise it.
    // MUTATION EVIDENCE: `true`→`false` at send.rs:run_send_pty_unified call site reds this.
    #[test]
    fn unified_pty_entry_wires_strict_true() {
        let src = include_str!("send.rs");
        let fn_start = src
            .find("pub(super) fn run_send_pty_unified(")
            .expect("run_send_pty_unified must exist");
        let after_fn = &src[fn_start..];
        // Scope to the function body: run_send_pty_resolved immediately follows.
        let fn_end = after_fn
            .find("\nfn run_send_pty_resolved(")
            .expect("run_send_pty_resolved must follow run_send_pty_unified");
        let body = &after_fn[..fn_end];

        // Must call run_send_pty_resolved with `true` as the final (strict) argument.
        assert!(
            body.contains("run_send_pty_resolved("),
            "run_send_pty_unified must call run_send_pty_resolved"
        );
        // The call ends with `true)` — the strict=true argument.
        assert!(
            body.contains(", true)"),
            "run_send_pty_unified must pass `true` (strict=true) to run_send_pty_resolved. \
             This pins QS-3 at the unified entry. Body:\n{body}"
        );
        // Negative control: must NOT pass false (which would be the send:pty compat call).
        assert!(
            !body.contains(", false)"),
            "run_send_pty_unified must NOT pass `false` (strict=false). Body:\n{body}"
        );
    }
}
