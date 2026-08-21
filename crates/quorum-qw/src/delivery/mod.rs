//! `delivery` — the five carrier cores, the sixth body no carrier reaches, and
//! the ledger emitters they share.
//!
//! **No CARRIER here prints, and none of them exits.** Every refusal is a typed
//! error carrying FACTS; every non-refusal line a carrier used to write mid-flow
//! is returned as a [`Notes`] entry. What prints is [`render`], the callers'
//! shared half — see its docs for why it lives here rather than in `qd`, and for
//! the one line in it that must move again when the wire lands. The shape is the
//! one [`crate::provider::claude::pty::revive`] established and `023ddf76`
//! ratified: a core that answers, a caller that prints.
//!
//! ── WHY THESE MOVED ─────────────────────────────────────────────────────────
//! `crate::lanes::Carriers` was a CALLBACK: `LaneOps::deliver` reached UP into
//! the `qd` binary's verb bodies to actually deliver a message. That edge points
//! the wrong way — `deliver` cannot run inside a `qw` process while its bodies are
//! `qd` verb functions — and it stranded sixteen qw-owned event emitters inside
//! qd's verbs, which independently blocked the ledger split. Relocating the bodies
//! retires the callback AND puts every qw-side emitter on qw's side of the
//! boundary. See `doc/tbd/provider-architecture/11-stage3-plan.md` phase 3B.
//!
//! The fifth carrier, `mux_pty`, landed last and lives in [`pty`]. Its printing
//! WAS interleaved through its control flow across 25 return sites, so it is cut
//! at the `wait`/`raw`/`full` boundary rather than at the function boundary. With
//! it, `Carriers` is GONE: nothing in `qw` calls into the `qd` binary.
//!
//! ── THE TWO BODIES THAT ARRIVED AFTER THE CARRIERS ──────────────────────────
//! Both were qw-owned delivery work still running out of a `qd` verb, and both
//! came here for that reason rather than because a lane call reaches them:
//!
//! - [`pty::run_anchor_wait`] — the other side of the `wait`/`raw`/`full` cut.
//!   The `--wait` loop's mid-line `eprint!`s and per-poll glyphs still cannot be
//!   [`Notes`], so the banner and the five closing words stay in the verb; what
//!   moved is everything the loop WROTE — its writer, its `status-transition`
//!   emitter, its `WatchGuard` and its `turn-anchored`.
//! - [`priming`] — `qd start -p`'s first turn. It is not any lane's `deliver`,
//!   and `LaneOps::start` refuses a create-time prompt for claude on a stated
//!   ruling; that module's header carries the evidence for both.
//!
//! ── THE TWO KINDS OF LINE A CARRIER PRODUCES ────────────────────────────────
//! A carrier's REFUSAL is one line and one exit code: that is the typed error.
//! But four of the five also write lines that are not refusals — pi's
//! landed/queued/pending confirmation, the floor's observable degrade log, ACP's
//! identity-preservation record, and the pane's eleven WARNINGs about a composer
//! that may still hold the text or a transcript that never showed it — and those
//! are interleaved with the work rather than returned from it. They come back as
//! [`Notes`], in emission order, on BOTH the success and the refusal side. The
//! caller prints notes first, then the outcome. Within stderr that reproduces the
//! pre-move order exactly, with ONE stated exception recorded on
//! [`pi::send_pi_floor`]: the floor's drop-log line used to lead the floor child's
//! inherited stderr and now trails it.
//!
//! Two of [`pty`]'s refusals print NO line of their own, because their whole
//! account was already written as a note — that is what [`CarrierError::line`]'s
//! `Option` encodes, and it is the only thing the pane carrier needed the shared
//! contract to widen for.
//!
//! stdout is [`Delivered::stdout`], and it is NOT a note. For the four resident
//! carriers it is the `CarrierOutcome::message_id` the lane already carries; for
//! [`pty`] it is a receipt sentence ("Message sent to …"). Per the split's rule
//! the caller prints it, and [`render`] is that caller.

use crate::effects::{Clock, Env};
use crate::model::Session;
use crate::paths::QdPaths;

pub mod acp;
pub mod acp_loss;
pub mod codex;
pub mod pi;
pub mod priming;
pub mod pty;
pub mod relay;

// ===========================================================================
// What a carrier core answers with
// ===========================================================================

/// stderr lines a carrier produced that are NOT its outcome line, in emission
/// order. The caller prints them verbatim, before anything else it prints.
///
/// This is what replaces the mid-flow `eprintln!`s the pre-move bodies used. A
/// library that prints cannot be called from a wire server, and a library that
/// silently drops these lines would be a behaviour change — so they travel.
pub type Notes = Vec<String>;

/// A carrier delivered. `message_id` is the id the lane keys its `Receipt` on and
/// the same value `Payload::SendInitiated.send_id` carries in the ledger.
pub struct Delivered {
    /// The relay message id / resident turn id / floor-minted send id.
    pub message_id: String,
    /// The one line the caller writes to **stdout**, or `None` when this carrier
    /// has never printed one.
    ///
    /// For every resident lane this is [`Delivered::message_id`] — the async-send
    /// analog echoes the id. It is `None` for pi's dead-only structured floor,
    /// which is a SYNCHRONOUS one-shot: it reports on stderr and has never printed
    /// an id, because the id it carries is one it minted for the ledger rather
    /// than one a resident handed back. And for [`pty`] it is a RECEIPT SENTENCE
    /// ("Message sent to …") rather than an id at all — the pane carrier has never
    /// echoed its `send_id`. Carrying the LINE rather than a bool is what lets
    /// those three answers coexist without the print site re-deriving them from
    /// the lane.
    pub stdout: Option<String>,
    /// See [`Notes`].
    pub notes: Notes,
}

/// A carrier refused: the typed reason, plus any notes it had already produced.
pub struct Refused<E> {
    /// See [`Notes`]. Printed BEFORE the refusal line, which is where they were
    /// written pre-move.
    pub notes: Notes,
    /// The refusal itself.
    pub error: E,
    /// The id this delivery had ALREADY minted when it failed, if any.
    ///
    /// A failed delivery that got as far as minting an id is still KEYED — that is
    /// what a later `qd delivery:recover` searches on — so the id survives the
    /// refusal instead of being dropped with it. The four daemon carriers mint
    /// theirs from the resident's reply, so every one of their refusals precedes an
    /// id and they all leave this `None` through the [`From`] impl below. [`pty`]
    /// mints its own before the first byte is written, so every refusal after that
    /// point carries one — see [`crate::lanes::CarrierOutcome::keyed`].
    pub message_id: Option<String>,
}

impl<E> From<E> for Refused<E> {
    fn from(error: E) -> Self {
        Refused {
            notes: Notes::new(),
            error,
            message_id: None,
        }
    }
}

/// The rendering contract every carrier error satisfies.
///
/// DELIBERATELY NO `Display` on any implementor. Most of these lines are
/// `qd <verb>:`-attributed and the verb is the CALLER's — the same bug
/// [`crate::provider::claude::pty::revive::ReviveClaudeError`] documents at
/// length. A `Display` would let a caller print a line with no verb, or with the
/// wrong one, by accident; [`line`](CarrierError::line) makes the verb impossible
/// to omit. Variants whose line never carried a `qd <verb>:` prefix ignore the
/// argument, which is the honest encoding of "this line was never
/// verb-attributed".
pub trait CarrierError {
    /// The complete stderr line, with the CALLER's verb stamped in where the line
    /// is verb-attributed.
    ///
    /// `None` for a refusal that prints NOTHING of its own, because its whole
    /// account was already delivered as a [`Notes`] entry earlier in the flow.
    /// Only [`pty`] has any: two of its strict-mode exits are the second half of a
    /// warning it had already written, and re-printing that warning as a refusal
    /// line would put a line on stderr that the pre-move body never wrote. A
    /// carrier whose every variant speaks returns `Some` from every arm.
    fn line(&self, verb: &str) -> Option<String>;
    /// The process exit code this refusal produces.
    fn exit_code(&self) -> i32;
}

/// The result shape all four carrier cores answer with.
pub type CarrierResult<E> = Result<Delivered, Refused<E>>;

/// Print a carrier core's answer and reduce it to a [`crate::lanes::CarrierOutcome`].
///
/// **THE ONE PRINTING SITE** for all five carriers. Every caller reaches it —
/// `qd send:relay`'s verb wrappers in `bin/qd/verbs/send_relay.rs`, the
/// `qd send:pty` shell in `bin/qd/verbs/send.rs`, and
/// [`crate::contract::LaneOps::deliver`]'s seven arms — so no two paths can
/// drift, which is precisely the property the retired `Carriers` callback used to
/// buy by pointing both at one function.
///
/// Notes come first, in emission order, because that is where the pre-move bodies
/// wrote them: interleaved with the work, ahead of the line that ended it.
///
/// ── WHY A qw MODULE PRINTS AT ALL, AND WHAT RETIRES IT ──────────────────────
/// The five CORES do not print — that is the whole point of them. This function
/// is the caller's half, and it lives here rather than in `qd` because
/// `deliver`'s arms are a caller too, and a `qd`-side twin would have to be
/// reached by a callback: the exact edge phase 3B exists to delete.
///
/// **stderr survives the process cut as-is.** Per `11-stage3-plan.md` D5 the `qw`
/// child inherits stderr precisely so diagnostics reach the user without
/// polluting the protocol stream, so every `eprintln!` below is already in its
/// final home.
///
/// **stdout does not, and [`Delivered::stdout`] is the line that must move
/// again.** Under the wire, stdout IS the protocol, so a carrier's user-facing
/// stdout line belongs to the qd client — off `Receipt::message_id` for the four
/// that echo an id, and off a rendering of the receipt for [`pty`]'s "Message
/// sent to …" sentence. It is printed here today because `deliver`'s arms are the
/// only caller that has the answer, and phase 4A is where the client gains one.
///
/// ── ORDERING, AND THE ONE THING THAT MOVED ──────────────────────────────────
/// Notes are stderr and [`Delivered::stdout`] is stdout, so ordering WITHIN each
/// stream is exactly the pre-move order. Ordering BETWEEN them is not, for the
/// two [`pty`] paths that used to print their receipt sentence and only then warn
/// (the busy-queued deferred verify, and every path's best-effort telemetry
/// append). Interleaved on a terminal those two lines now swap; captured
/// separately — which is how every test in this repo reads them — nothing moves.
/// Recorded rather than papered over, alongside the floor's drop-log line on
/// [`pi::send_pi_floor`].
/// Whether this process's stdout is the WIRE, not a terminal.
///
/// `false` in the `qd` binary, whose `send`/`send:relay` verbs call [`render`]
/// directly with a real terminal on stdout — there the carrier's line is the
/// user-facing answer and belongs exactly where it is. `true` inside `qw serve`,
/// which sets it via [`stdout_is_protocol`] before reading its first frame.
///
/// A process-global rather than a parameter because the fact it records is a
/// property of the PROCESS, not of a call: every carrier reached under `serve`
/// shares one stdout, and threading a sink through nine `deliver` arms and the
/// two `CarrierResult` shapes would restate the same answer at each of them.
static STDOUT_IS_PROTOCOL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Declare that stdout carries the protocol — see [`STDOUT_IS_PROTOCOL`].
///
/// Called once by [`crate::wire::server::serve`]. Idempotent, and never unset:
/// a process that has become a wire server does not go back to being a terminal.
pub fn stdout_is_protocol() {
    STDOUT_IS_PROTOCOL.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn render<E: CarrierError>(
    result: CarrierResult<E>,
    verb: &str,
) -> crate::lanes::CarrierOutcome {
    match result {
        Ok(d) => {
            render_notes(&d.notes);
            if let Some(line) = &d.stdout {
                // Under the wire this line would be read by qd as a FRAME —
                // `wire::server`'s stated invariant ("stdout is the protocol")
                // and the one place that broke it. The receipt already carries
                // `message_id`, so nothing is lost by routing the human line to
                // stderr, which qw inherits and the user still sees.
                if STDOUT_IS_PROTOCOL.load(std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
            crate::lanes::CarrierOutcome::keyed(0, d.message_id)
        }
        Err(r) => {
            render_notes(&r.notes);
            if let Some(line) = r.error.line(verb) {
                eprintln!("{line}");
            }
            match r.message_id {
                Some(id) => crate::lanes::CarrierOutcome::keyed(r.error.exit_code(), id),
                None => crate::lanes::CarrierOutcome::unkeyed(r.error.exit_code()),
            }
        }
    }
}

/// Write a carrier's [`Notes`] to stderr, in emission order.
///
/// Split out of [`render`] for the ONE caller that cannot use `render`: `qd
/// send:pty --wait`, whose delivery half returns [`pty::PtyOutcome::Await`] with
/// notes already accumulated and then keeps printing mid-line itself. Having it
/// here rather than as a `for` loop in the verb keeps "how a note prints" a single
/// decision even where "what a carrier answers" could not be.
pub fn render_notes(notes: &Notes) {
    for note in notes {
        eprintln!("{note}");
    }
}

// ===========================================================================
// The injected effects
// ===========================================================================

/// Everything a daemon carrier cannot own, resolved by the caller and handed in.
///
/// `paths` is the caller's `paths_from_home` result — the `.claude`-layout
/// `QdPaths` the registry/socket lookups key on. It is deliberately NOT the
/// QD_HOME-honouring `from_home_env` layout: the ledger emitters below re-derive
/// THAT one from `env` themselves, exactly as they did pre-move, because the
/// delivery log and the registry live under different roots.
pub struct SendDeps<'a> {
    pub env: &'a dyn Env,
    pub paths: &'a QdPaths,
    pub clock: &'a dyn Clock,
}

/// The owned inputs of one delivery.
pub struct SendParams<'a> {
    pub session: &'a Session,
    pub message: &'a str,
    /// The send id the CALLER minted, before the message crossed the boundary.
    ///
    /// See [`crate::contract::Message::id`] for why it arrives rather than being
    /// minted here. A carrier whose ledger key is its RESIDENT's turn id (the
    /// codex / `acp/*` / pi daemon arms and the claude relay) does not use it:
    /// their `send-initiated` is keyed on the id the resident answered with,
    /// which is also the id their stdout receipt carries, and inventing a second
    /// key for the same send would leave the observer-written terminal joining on
    /// neither. The two carriers that used to call `events::mint_send_id`
    /// themselves — the mux-pane carrier and pi's structured floor sub-lane —
    /// use this one.
    pub send_id: &'a str,
}

// ===========================================================================
// The ledger emitters — qw-owned payloads, previously written from qd verbs
// ===========================================================================

/// The delivery log the events go into, keyed to the TARGET (never the sender):
/// the target uuid when the caller resolved a row, else the `byname-<target>`
/// file (a consumer merges both, §1.4 G5). `None` when HOME is unset — emission
/// is best-effort and a missing state dir is not a send failure.
fn target_writer(
    env: &dyn Env,
    target_name: &str,
    target_session: Option<&Session>,
) -> Option<crate::events::EventWriter> {
    let home = env.var("HOME").filter(|s| !s.is_empty())?;
    let state_dir = QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    Some(match target_session {
        Some(s) => crate::events::EventWriter::for_key(
            &state_dir,
            &s.session_id,
            Some(s.session_id.clone()),
            s.name.clone(),
        ),
        None => crate::events::EventWriter::for_key(
            &state_dir,
            &crate::events::byname_key(target_name),
            None,
            Some(target_name.to_string()),
        ),
    })
}

/// §X (3-phase delivery) — relay on-sent + on-queued emission.
///
/// Writes `send-initiated` (the EXISTING `Payload::SendInitiated` constructed
/// with relay values, §X.3.1 — NOT a bare 2-field record) and `relay-delivered`
/// (§X.3.2, non-terminal) into the **TARGET's** delivery log. `send_id =
/// message_id`; `content_sha256 = sha256(raw caller message bytes)` (§X.4 — the
/// SAME bytes the consumer hashes into its own on-sent marker). The relay
/// `send-initiated` carries NO prose (`content_preview` omitted — a privacy
/// improvement, §X.7).
///
/// BEST-EFFORT: a write failure (or an unresolvable HOME) is logged by
/// `warn_emit` and NEVER affects the send result — the message already left and
/// the relay WIRE is untouched.
pub fn emit_relay_send_events(
    env: &dyn Env,
    clock: &dyn Clock,
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    message_id: &str,
) {
    let Some(writer) = target_writer(env, target_name, target_session) else {
        return;
    };
    let content_sha256 = crate::events::sha256_hex(message.as_bytes());

    // on-sent — REUSE Payload::SendInitiated with the §X.3.1 relay values.
    crate::events::warn_emit(
        &writer,
        clock,
        &crate::events::Payload::SendInitiated {
            send_id: message_id.to_string(),
            verb: "send:relay".to_string(),
            send_path: "relay".to_string(),
            content_sha256: content_sha256.clone(),
            content_len: message.as_bytes().len() as u64,
            chunks: 1,
            chunk_sha256s: vec![content_sha256.clone()],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        },
    );
    // on-queued — relay-delivered (§X.3.2), NON-terminal.
    crate::events::warn_emit(
        &writer,
        clock,
        &crate::events::Payload::RelayDelivered {
            send_id: message_id.to_string(),
            content_sha256,
        },
    );
}

/// C5/C3 (3-phase delivery, DAEMON lanes) — emit the SENT + DELIVERED phases into
/// the TARGET's log on inject-SUCCESS, for the codex / `acp/*` / pi resident arms.
/// `send-initiated` (sent — the REUSED `Payload::SendInitiated` with daemon values:
/// verb `send:relay`, `send_path` the lane, `send_id` the resident-minted turn id)
/// + `turn-accepted` (delivered, NON-terminal — the resident took the prompt as a
/// turn). The success TERMINAL lands later at the OBSERVATION seam (`run_acp_wait`
/// StopReason-mapped; the pi content-keyed rollout observer), NEVER here — so
/// between this and observation the send reads as non-terminal DELIVERED =
/// PENDING (gate item 3), honest.
///
/// NO `transcript`/`transcript_offset` recovery keys are carried: a resident send
/// is OBSERVER-closed, and `qd delivery:recover`'s sweep is verb-gated to
/// {`send:pty`, `new-p`} — so this `send-initiated` (verb `send:relay`) is out of
/// that sweep by construction and can never be mistaken for a
/// transcript-recoverable pty dangling. `content_sha256` = sha256(raw message),
/// the SAME key the observer/consumer matches on. BEST-EFFORT: a write failure is
/// logged by `warn_emit` and never affects the send result.
pub fn emit_daemon_send_events(
    env: &dyn Env,
    clock: &dyn Clock,
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    turn_id: &str,
    send_path: &str,
) {
    let Some(writer) = target_writer(env, target_name, target_session) else {
        return;
    };
    let content_sha256 = crate::events::sha256_hex(message.as_bytes());
    // sent — REUSE Payload::SendInitiated with daemon values (no recovery keys).
    crate::events::warn_emit(
        &writer,
        clock,
        &crate::events::Payload::SendInitiated {
            send_id: turn_id.to_string(),
            verb: "send:relay".to_string(),
            send_path: send_path.to_string(),
            content_sha256: content_sha256.clone(),
            content_len: message.as_bytes().len() as u64,
            chunks: 1,
            chunk_sha256s: vec![content_sha256.clone()],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        },
    );
    // delivered — turn-accepted (NON-terminal): the resident accepted the turn.
    crate::events::warn_emit(
        &writer,
        clock,
        &crate::events::Payload::TurnAccepted {
            send_id: turn_id.to_string(),
            content_sha256,
        },
    );
}

/// C5/C3 — emit the daemon-lane success TERMINAL `message-seen{send_id,
/// content_sha256}` (the FLOOR / record-presence reading) into the TARGET's log,
/// best-effort. Used by the pi structured FLOOR ([`pi::send_pi_floor`], the
/// dead-only sub-lane) once the sent bytes are confirmed present in the appended
/// session record (content-keyed). The RESIDENT lanes emit their terminal through
/// the content-keyed observer ([`pi::observe_landed_sends`]) instead, never here.
/// A reader recovers the floor-vs-strong reading from the paired send-initiated's
/// send_path + D4's table.
pub fn emit_daemon_seen(
    env: &dyn Env,
    clock: &dyn Clock,
    target_name: &str,
    target_session: Option<&Session>,
    send_id: &str,
    content_sha256: &str,
) {
    let Some(writer) = target_writer(env, target_name, target_session) else {
        return;
    };
    crate::events::warn_emit(
        &writer,
        clock,
        &crate::events::Payload::MessageSeen {
            send_id: send_id.to_string(),
            content_sha256: content_sha256.to_string(),
        },
    );
}

/// §C1 — emit a single `send-failed` terminal at a send DOOR (best-effort).
/// Serves BOTH the relay door (`no_relay_exit`, D1) AND the daemon protocol arms
/// (codex/`acp/*`/pi — [`codex::send_codex`], [`acp::send_acp`], [`pi::send_pi`],
/// [`pi::send_pi_floor`]): every carrier's door failure funnels its `send-failed`
/// here, so no door reads as "someone else's child" (door-inventory §A/§B).
/// Mirrors [`emit_relay_send_events`]'s target-keying and content hashing but
/// emits ONE failure record instead of the success pair. `send_id` is OMITTED
/// (spec `send-failed { send_id?, reason }`): a resolved pre-wire door has no
/// server-minted / resident-minted id yet — never client-invented. `content_sha256`
/// is carried for correlation; `reason` is a short surface token (extend additively).
pub fn emit_door_failure(
    env: &dyn Env,
    clock: &dyn Clock,
    target_name: &str,
    target_session: Option<&Session>,
    message: &str,
    reason: &str,
) {
    let Some(writer) = target_writer(env, target_name, target_session) else {
        return;
    };
    let content_sha256 = crate::events::sha256_hex(message.as_bytes());
    crate::events::warn_emit(
        &writer,
        clock,
        &crate::events::Payload::SendFailed {
            send_id: None,
            content_sha256,
            reason: reason.to_string(),
        },
    );
}

// ===========================================================================
// Shared derivations
// ===========================================================================

/// B2 item 5 — derive the `from_session` channel-header identity. Ratified
/// precedence (Q3; the from_session NAMESPACE is the claude session uuid —
/// reply routing keys on it, so step 1 RESOLVES to a uuid, never emits the
/// stable id itself):
///
///   1. ENGINE-ASSERTED: `QD_SESSION_ID` — the engine birth property,
///      explicitly set at every launch (override-never-inherit, the D1 site-4
///      lesson) — resolved through the idstore to the claude uuid.
///   2. `CLAUDE_CODE_SESSION_ID` — only when NO engine identity resolves
///      (bare-shell operator sends from inside a claude session; also the
///      pre-fix inherited-env channel, now demoted so a leaked env var from a
///      different session can no longer mis-attribute an engine session's
///      sends — the punch_b2_item5_repro pin).
///   3. `"cli"` — bare operator shell.
///
/// An QD_SESSION_ID that is malformed, unknown to the store, or still UNBOUND
/// (mint without a session uuid yet) falls through to (2) — the derivation
/// never invents an identity. Cost: one `ids.jsonl` read per send (accepted
/// at the phase-2 checkpoint; `whoami` pays the same read).
///
/// ── WHO SPENDS THIS, AND WHAT `"cli"` MEANS TO THEM (punch R10) ─────────────
/// The relay lane hands it to the relay server, which renders the
/// `<channel source="relay" from_session=…>` header a claude session reads. The
/// TEXT-ONLY lanes (codex, pi) have no header to put it in, so their `inject`
/// renders it INTO the message with
/// [`crate::provider::shared::attribution::attribute`]. Both spellings agree on
/// step (3): `"cli"` is a bare operator shell, NOT a peer — there is no session
/// to answer, so the text-only lanes emit no envelope at all rather than name an
/// unreachable sender. That is why `"cli"` must stay a value this function can
/// RETURN rather than an error it refuses: it is a real, common answer.
pub fn derive_from_session(env: &dyn Env) -> String {
    if let Some(stable) = env.var("QD_SESSION_ID").filter(|s| !s.is_empty()) {
        if let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) {
            let paths = QdPaths::from_home_env(std::path::Path::new(&home), env);
            let ids = crate::idstore::fold(&crate::idstore::ids_path(&paths.state_dir));
            // The SHARED resolution chain (S4): whoami and attribution answer
            // "what engine identity resolves?" identically by construction.
            if let Some(uuid) = crate::idstore::resolve_to_uuid(&ids, &stable) {
                return uuid;
            }
        }
    }
    env.var("CLAUDE_CODE_SESSION_ID")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_string())
}

/// Append a content-free A6 invoked line for a SUCCESSFUL send. The relay fast
/// path yields only a NAME (no sessionId), so the line is keyed by name — the
/// fold accepts either. Best-effort: a failure produces a WARNING note and NEVER
/// changes the verb's exit code (spec §4.1).
///
/// Returns the note rather than printing it — this is a library.
pub fn append_send_invoked(env: &dyn Env, clock: &dyn Clock, name: &str) -> Option<String> {
    match crate::telemetry::append_invoked(env, clock, "send", None, Some(name)) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "WARNING: telemetry invoked append failed (non-fatal): {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use crate::events::{parse_events, sha256_hex};
    use crate::model::{Session, SessionBranch, SessionStatus};

    /// A `Clock` pinned to a fixed instant — the emitters take one, and nothing
    /// asserted here depends on the wall reading.
    struct FixedClock;
    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            1_770_000_000_000
        }
    }

    fn blank_session() -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: String::new(),
            code: None,
            qd_id: None,
            pid: None,
            status: SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: String::new(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    fn env_for(home: &std::path::Path) -> MapEnv {
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
        MapEnv { vars, uid: 501 }
    }

    /// D2 §C1 (door-inventory §B) — a daemon-arm DOOR FAILURE emits exactly one
    /// `send-failed` terminal into the TARGET's delivery log: keyed to the target
    /// uuid, `send_id` OMITTED (pre-wire, spec `send-failed { send_id? }`),
    /// `content_sha256` over the raw message, `reason` the surface token. This is
    /// the single emission ALL four daemon-arm doors (codex/acp/pi/floor) funnel
    /// through; the per-lane end-to-end door INJECTIONS live in the conformance
    /// suite. No record is hand-written.
    #[test]
    fn door_failure_emits_send_failed_terminal_keyed_to_target() {
        let home = tempfile::tempdir().unwrap();
        let env = env_for(home.path());
        let target_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let message = "steer: please ack";
        let target = Session {
            name: Some("codex-dead-1".to_string()),
            session_id: target_uuid.to_string(),
            provider: "codex".to_string(),
            ..blank_session()
        };
        emit_door_failure(
            &env,
            &FixedClock,
            "codex-dead-1",
            Some(&target),
            message,
            "daemon-unreachable",
        );

        let state_dir = QdPaths::from_home_env(home.path(), &env).state_dir;
        let log = state_dir
            .join("sessions")
            .join(format!("{target_uuid}.events.jsonl"));
        let raw = std::fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("target log {log:?} must exist: {e}"));
        let recs = parse_events(&raw).records;

        let kinds: Vec<&str> = recs.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["send-failed"],
            "exactly one send-failed at the door; got {kinds:?}"
        );

        let sf: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("send-failed")).unwrap()).unwrap();
        assert_eq!(sf["event"], "send-failed");
        assert_eq!(sf["v"], 1);
        assert_eq!(sf["reason"], "daemon-unreachable");
        assert_eq!(
            sf["content_sha256"].as_str().unwrap(),
            sha256_hex(message.as_bytes()),
            "content_sha256 over the raw message bytes (the consumer's join key)"
        );
        assert!(
            sf.get("send_id").is_none(),
            "send_id OMITTED at a pre-wire door (spec `send-failed {{ send_id? }}`)"
        );
        assert_eq!(
            sf["session"].as_str().unwrap(),
            target_uuid,
            "keyed to the TARGET uuid"
        );
        assert!(
            crate::events::is_terminal("send-failed"),
            "send-failed is a terminal (§C1)"
        );
    }

    /// D2 §C5/C3 (daemon-lane phases) — a daemon inject-SUCCESS emits exactly
    /// `send-initiated` (sent — daemon values: verb `send:relay`, `send_path` the
    /// lane, `send_id` the resident turn id, NO recovery transcript keys) then
    /// `turn-accepted` (delivered, NON-terminal), keyed to the TARGET uuid, in that
    /// order. The success TERMINAL is deliberately NOT emitted here — it lands at
    /// the observe seam. The absent transcript keys + verb `send:relay` keep this
    /// send OUT of the recover verb's pty/new-p sweep.
    #[test]
    fn daemon_send_emits_send_initiated_then_turn_accepted() {
        let home = tempfile::tempdir().unwrap();
        let env = env_for(home.path());
        let target_uuid = "12121212-3434-5656-7878-909090909090";
        let message = "steer: land this mid-turn";
        let turn_id = "turn-abc";
        let target = Session {
            name: Some("acp-live-1".to_string()),
            session_id: target_uuid.to_string(),
            pid: Some(4242),
            provider: "acp/claude-code".to_string(),
            ..blank_session()
        };
        emit_daemon_send_events(
            &env,
            &FixedClock,
            "acp-live-1",
            Some(&target),
            message,
            turn_id,
            "acp/claude-code",
        );

        let state_dir = QdPaths::from_home_env(home.path(), &env).state_dir;
        let log = state_dir
            .join("sessions")
            .join(format!("{target_uuid}.events.jsonl"));
        let raw = std::fs::read_to_string(&log)
            .unwrap_or_else(|e| panic!("target log {log:?} must exist: {e}"));
        let recs = parse_events(&raw).records;

        let kinds: Vec<&str> = recs.iter().map(|r| r.event.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["send-initiated", "turn-accepted"],
            "daemon inject-success emits sent then delivered (no terminal here); got {kinds:?}"
        );
        for r in &recs {
            assert_eq!(
                r.send_id().as_deref(),
                Some(turn_id),
                "send_id == the resident turn id on both records"
            );
        }
        let want_sha = sha256_hex(message.as_bytes());

        let si: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("send-initiated")).unwrap())
                .unwrap();
        assert_eq!(si["verb"], "send:relay");
        assert_eq!(si["send_path"], "acp/claude-code", "send_path names the lane");
        assert_eq!(si["content_sha256"].as_str().unwrap(), want_sha);
        assert!(
            si.get("transcript").is_none() && si.get("transcript_offset").is_none(),
            "NO recovery transcript keys → out of the recover verb's pty/new-p sweep"
        );
        assert_eq!(
            si["session"].as_str().unwrap(),
            target_uuid,
            "keyed to TARGET uuid"
        );

        let ta: serde_json::Value =
            serde_json::from_str(raw.lines().find(|l| l.contains("turn-accepted")).unwrap())
                .unwrap();
        assert_eq!(ta["event"], "turn-accepted");
        assert_eq!(ta["content_sha256"].as_str().unwrap(), want_sha);
        assert!(
            !crate::events::is_terminal("turn-accepted"),
            "turn-accepted is NON-terminal delivered — only the terminal says landed"
        );
    }

    // ---- the B2-item-5 identity derivation ---------------------------------

    /// Build a MapEnv + a staged ids.jsonl under a tempdir HOME. Returns the
    /// tempdir (keep alive) and the env.
    fn identity_env(
        qd_session_id: Option<&str>,
        claude_session_id: Option<&str>,
        ids_lines: &str,
    ) -> (tempfile::TempDir, MapEnv) {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".quorum").join("dispatch").join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("ids.jsonl"), ids_lines).unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.path().to_string_lossy().to_string());
        if let Some(v) = qd_session_id {
            vars.insert("QD_SESSION_ID".to_string(), v.to_string());
        }
        if let Some(v) = claude_session_id {
            vars.insert("CLAUDE_CODE_SESSION_ID".to_string(), v.to_string());
        }
        (home, MapEnv { vars, uid: 501 })
    }

    const MINT: &str = "{\"event\":\"mint\",\"id\":\"ab3kx9mq\",\"session_id\":\"true-uuid-1\"}\n";

    #[test]
    fn engine_identity_wins_over_inherited_env() {
        // Both planted: the idstore-resolved uuid wins over the leaked env var.
        let (_h, env) = identity_env(Some("ab3kx9mq"), Some("imposter-uuid"), MINT);
        assert_eq!(derive_from_session(&env), "true-uuid-1");
    }

    #[test]
    fn unresolvable_engine_identity_falls_back_to_claude_env() {
        // Valid-shaped but unknown id → fall through, never invent.
        let (_h, env) = identity_env(Some("zzzzzzzz"), Some("cc-uuid"), MINT);
        assert_eq!(derive_from_session(&env), "cc-uuid");
        // Malformed id → same fall-through.
        let (_h2, env2) = identity_env(Some("not-an-id!"), Some("cc-uuid"), MINT);
        assert_eq!(derive_from_session(&env2), "cc-uuid");
        // UNBOUND mint (no uuid yet) → same fall-through.
        let unbound = "{\"event\":\"mint\",\"id\":\"cd47qrst\",\"session_id\":null}\n";
        let (_h3, env3) = identity_env(Some("cd47qrst"), Some("cc-uuid"), unbound);
        assert_eq!(derive_from_session(&env3), "cc-uuid");
    }

    #[test]
    fn bare_shell_is_cli() {
        // Neither identity present → "cli" (the operator-shell attribution the
        // ruling pins as must-not-break).
        let (_h, env) = identity_env(None, None, MINT);
        assert_eq!(derive_from_session(&env), "cli");
        // QD_SESSION_ID unresolvable and no claude env → still "cli".
        let (_h2, env2) = identity_env(Some("zzzzzzzz"), None, MINT);
        assert_eq!(derive_from_session(&env2), "cli");
    }

    #[test]
    fn engine_identity_is_case_folded() {
        // Ids are case-insensitive at resolution (idstore::normalize).
        let (_h, env) = identity_env(Some("AB3KX9MQ"), Some("imposter"), MINT);
        assert_eq!(derive_from_session(&env), "true-uuid-1");
    }
}
