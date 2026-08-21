//! **qd's half of the delivery ledger** — the intent log
//! (`doc/tbd/provider-architecture/09-ledger-split.md`).
//!
//! The ruling is two logs, and qd never reads qw's. qw's
//! (`<state>/sessions/<key>.events.jsonl`) holds delivery and terminal; this one
//! (`<state>/intent/<key>.events.jsonl`) holds what qd was ASKED to send, with
//! qd's own process identity on the envelope.
//!
//! ## Why the record is written before the wire, not after
//!
//! `qd delivery:recover` closes DEAD-DANGLING sends: a `send-initiated` whose
//! writer incarnation is gone. Its sweep enumerates qd's own records, and clauses
//! (b) "the writer incarnation is gone" and (c) "age > `T_ANCHOR_IDLE_MS`" are
//! read off THIS record's `pid`/`start_ms`/`ts`. A send whose delivery hung and
//! whose writer was then killed is exactly the case that verb exists for — and if
//! the intent record were appended after `deliver` returned, that send would leave
//! no trace at all and the sweep would have nothing to find.
//!
//! So the id is minted here, the record is durable, and only then does the
//! message cross. It is the same write-then-deliver discipline `send_unified`
//! already follows for its disposition envelope, applied one layer down.
//!
//! ## Why both sides write a `send-initiated`, and why that is not duplication
//!
//! qd's records **what was asked**, with qd's process identity and no recovery
//! keys — qd resolves no transcripts and must not start. qw's records **what was
//! attempted**, keyed on the `content_sha256` and transcript offset it will later
//! resolve the send against. `09-ledger-split.md` says exactly this: "qd passes no
//! recovery keys, because qw can record its own `content_sha256`/offset at
//! delivery time." Two facts, two owners, two files.
//!
//! ## The one file in `qd` that may name the intent log
//!
//! Every intent read AND write goes through here, so `crate::ledger_gate` can pin
//! the ledger call sites by file — this module is the only qd file that may name
//! either log's reader, and it names the INTENT one.
//! `EventWriter::for_intent` is a separate constructor from `for_key` for the
//! same reason: which log a write lands in is then a fact a source scan can read,
//! rather than a value in a variable.

use std::path::Path;

use dispatch::effects::{Clock, Env};
use dispatch::events::{self, EventRecord, EventWriter, Payload};
use dispatch::paths::QdPaths;

/// The verb token qd stamps on a unified `qd send`'s intent record.
///
/// **Not `"send:pty"`**, and the difference is load-bearing. `LaneOps::deliver`
/// chooses the carrier privately and deliberately does not tell qd which one it
/// picked, so at intent time qd does not know whether this send will be
/// transcript-anchored. Stamping `send:pty` would be a claim about a carrier qd
/// did not choose.
///
/// `verbs/recover.rs`'s sweep therefore accepts this token alongside `send:pty`
/// and `new-p`: a unified send that ends up on the pane carrier must stay
/// recoverable (qw's record carries the same id, so `recover` resolves it
/// normally), and one that ends up on a resident carrier answers
/// `Undetermined` — qw's ledger has no `send-initiated` under this id, because a
/// resident keys its own on the turn id it minted — which mints no terminal and
/// closes nothing. The scoping defence the sweep exists for is unaffected:
/// `send:relay` is still not swept.
pub const VERB_SEND: &str = "send";

/// The `send_path` every qd intent record carries.
///
/// On qw's records this field names an OBSERVATION — `"idle"` / `"busy-queued"`
/// for the pane carrier, the lane string for the resident ones — and qd made
/// none of those observations. Whether the target was mid-turn is exactly the
/// kind of session state `LaneOps::deliver` looks at privately and does not
/// report back, so writing `"idle"` here would be qd asserting something it never
/// read. This names what the record IS instead.
pub const SEND_PATH_INTENT: &str = "intent";

/// Mint a send id and record the intent, returning the id.
///
/// The id is [`events::mint_send_id`]'s `"{pid}-{epoch_ms}-{n}"`, unchanged in
/// shape. What changed with the split is whose pid is in it: the mint moved out
/// of the carrier and up to the caller, so it now carries **qd's** pid rather
/// than the `qw` subprocess's. Nothing parses a send id — §2.1 makes it opaque,
/// equality only — and the dead-writer rule reads the ENVELOPE pid, never this
/// one, which is why the move is safe.
///
/// Emission is best-effort in exactly the sense the rest of the ledger is: an
/// unresolvable `HOME` or a failed append warns and returns the id anyway. A send
/// is not failed because its forensics could not be written — but it IS attempted
/// with an id, so a later reader that finds qw's record can still name it.
pub fn record_send_intent(
    env: &dyn Env,
    clock: &dyn Clock,
    session_id: Option<&str>,
    name: Option<&str>,
    verb: &str,
    message: &str,
) -> String {
    let send_id = events::mint_send_id(clock);
    // Keyed to the TARGET, sessionId when one is resolved else `byname-<name>` —
    // the SAME key rule qw's delivery log uses, so the two halves of one send sit
    // under matching names in the two trees and `intent_reader_paths` merges the
    // pair exactly as `reader_paths` does.
    let (key, session, target) = match (session_id.filter(|s| !s.is_empty()), name) {
        (Some(sid), n) => (
            sid.to_string(),
            Some(sid.to_string()),
            n.map(str::to_string),
        ),
        (None, Some(n)) => (events::byname_key(n), None, Some(n.to_string())),
        // Neither key: nothing to file it under, so there is nothing to write.
        // The send still gets its id — a record we cannot key is not a reason to
        // deliver anonymously.
        (None, None) => return send_id,
    };
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return send_id;
    };
    let state_dir = QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    let writer = EventWriter::for_intent(&state_dir, &key, session, target);
    let content_sha256 = events::sha256_hex(message.as_bytes());
    events::warn_emit(
        &writer,
        clock,
        &Payload::SendInitiated {
            send_id: send_id.clone(),
            verb: verb.to_string(),
            send_path: SEND_PATH_INTENT.to_string(),
            content_sha256: content_sha256.clone(),
            content_len: message.as_bytes().len() as u64,
            // qd does not chunk — the pane carrier does, and its own record
            // carries the real per-chunk shas. One "chunk" here is the whole
            // message as qd was handed it, which is the only splitting qd
            // performed.
            chunks: 1,
            chunk_sha256s: vec![content_sha256],
            chunk_sha256s_capped: false,
            // NO recovery keys, by ruling: resolving a transcript is
            // session-artifact access and qd never opens one.
            transcript: None,
            transcript_offset: None,
            // No prose in the intent log. The preview exists so an operator
            // reading the DELIVERY log can tell which send a terminal belongs to;
            // duplicating the text into a second file buys nothing and doubles
            // the surface a redaction bug can leak through.
            content_preview: None,
        },
    );
    send_id
}

/// Every `send-initiated` record in qd's intent tree whose `verb` is in `verbs`,
/// optionally narrowed to one `send_id`.
///
/// The counterpart of [`record_send_intent`], and here for the same reason: the
/// intent log's layout is known in exactly one qd module. A missing directory is
/// an empty answer, never an error — a state dir that has never sent anything has
/// nothing to recover, which is not a failure.
///
/// `events::parse_events` is the shared vocabulary parser
/// (`quorum-delivery-events` owns the record schema; both logs are written with
/// it), applied here to **qd's own files**. The directory is
/// [`events::intent_dir`]; `events::events_dir` is never named.
pub fn scan(state_dir: &Path, verbs: &[&str], target: Option<&str>) -> Vec<EventRecord> {
    let dir = events::intent_dir(state_dir);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_events_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".events.jsonl"))
            .unwrap_or(false);
        if !is_events_file {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for rec in events::parse_events(&text).records {
            if rec.event != "send-initiated" {
                continue;
            }
            match rec.str_field("verb").as_deref() {
                Some(v) if verbs.contains(&v) => {}
                _ => continue,
            }
            if let Some(t) = target {
                if rec.send_id().as_deref() != Some(t) {
                    continue;
                }
            }
            out.push(rec);
        }
    }
    out
}
