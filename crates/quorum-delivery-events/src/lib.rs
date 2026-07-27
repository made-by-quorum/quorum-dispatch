//! Shared delivery-event VOCABULARY (vocab-extract from `dispatch::events`).
//!
//! This LEAF crate owns the producer surface of the ACK-2/ACK-3 engine event
//! schema: the `Payload` enum (the 19 event kinds), the `Envelope`/`Anchor`
//! records, the normative terminal set, the record-schema constants, and the
//! byte-exact wire serializer ([`build_record_line`]). It depends on NEITHER
//! `dispatch` NOR `qrmux` so BOTH can emit the same vocabulary without a
//! dependency cycle (the existing edge is `dispatch → qrmux`).
//!
//! The writer (`EventWriter`/`emit`), the reader (`parse_events`/`read_merged`),
//! and ALL resolver logic (recovery-read, dead-writer rule, `await_received`)
//! stay in `dispatch::events`; they PRODUCE/RESOLVE *using* this vocab and import
//! it from here. `dispatch::events` re-exports every public name below so all
//! existing `dispatch::events::Foo` / `crate::events::Foo` call sites compile and
//! behave byte-for-byte unchanged.
//!
//! # Privacy (ack2-spec §9 privacy row, VERBATIM — do not weaken)
//!
//! Records carry `content_sha256` + `content_len` ONLY — never raw message text
//! (plus the redacted, ≤256 B `content_preview` on `send-initiated`; see the
//! dispatch-side redactor and `doc/EVENT-CONTRACT.md`). `chunk_sha256s` are shas
//! of content SUBSTRINGS. This is pseudonymization for high-entropy text, not a
//! one-way veil.
//!
//! # Wire byte-exactness (LANDMINE)
//!
//! [`build_record_line`] hand-builds a `serde_json::Map` by INSERTING keys in a
//! pinned order, then calls `.to_string()`. With serde_json's `preserve_order`
//! feature the Map is insertion-ordered (`IndexMap`); WITHOUT it, it is a
//! `BTreeMap` (sorted) → DIFFERENT BYTES. This crate gates `preserve_order`
//! behind the default-ON `json-insertion-order` feature (see Cargo.toml), exactly
//! as `dispatch` does. Do not force it unconditionally.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

// ===========================================================================
// §2.3 — content hash (producer wire surface)
// ===========================================================================

/// SHA-256 of `bytes` as 64-char lowercase hex (§2.3: "sha encoding = 64-char
/// lowercase hex of SHA-256, always"). Uses the existing `sha2` dep.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ===========================================================================
// §3 — CR-2 normative terminal set
// ===========================================================================

/// THE terminal set (ack2-spec §3 — rev C §2.1 reader-verdict rule + §2.4 await
/// contract). Consumers NEVER infer or extend it. `chunks-delivered` /
/// `composer-cleared` / `status-transition` can NEVER satisfy a wait (the
/// cheap-event trap stays structurally closed: no helper in this crate returns
/// success on a non-terminal event, and G4 proves it).
///
/// Note (§3): `anchor-timeout` is terminal FOR THE WATCH, not an immutable
/// delivery fact — a later recovery-read MAY append a late `turn-anchored` after
/// it; readers take the FIRST terminal in file-read order as the verdict.
pub const TERMINAL_EVENTS: [&str; 7] = [
    "turn-anchored",
    "turn-anchored-mismatch",
    "anchor-timeout",
    "pending-abandoned",
    // §X.3.4/§X.3.5 (3-phase delivery) — the on-received terminals. Added here so
    // first-terminal-wins per send_id covers them AND the §4.3 rotation reserve band
    // protects them (a terminal must never be rotated out). `relay-delivered` is
    // deliberately NOT here — it is non-terminal (the relay analog of chunks-delivered).
    "message-seen",
    "seen-failed",
    // §C1 (delivery contract) — the DOOR-failure terminal. Terminal so first-terminal-
    // wins resolves a failed send AND the rotation reserve band protects it (a failure
    // record must never be rotated out). A relay-door `send-failed` carries no send_id,
    // so it never joins by send_id (the door failure is synchronous — the invoker holds
    // the exit path); it is nonetheless a terminal-CLASS record for rotation purposes.
    "send-failed",
];

/// Is `event` a member of the normative terminal set (§3)? `await_received`
/// (§8), the dead-writer rule (§7) and recovery-read's dangling test (§6) ALL
/// call this — one definition, no parallel lists.
pub fn is_terminal(event: &str) -> bool {
    TERMINAL_EVENTS.contains(&event)
}

/// THE success sub-set of [`TERMINAL_EVENTS`]: the terminals meaning "the send
/// reached the recipient's context" (delivered), as opposed to a failure /
/// mismatch / abandonment terminal. Currently exactly `message-seen` (the
/// 3-phase on-received success; `turn-anchored` is the legacy ack path, not
/// emitted on the polite-delivery lane). This is the ONE HOME for the
/// "which terminal means delivered" identity: M2's banner (`toast_kind_for`),
/// M3's `--wait` reader classification, and any future consumer bind THIS —
/// never a locally-minted literal (F5/M2 de-dup; the anti-fork gate greps for
/// stray `"message-seen"` success literals). A success terminal is necessarily a
/// terminal, so callers may treat `is_success_terminal` as implying
/// [`is_terminal`]. Grow the set HERE (and only here) if a new success terminal
/// is minted.
pub fn is_success_terminal(event: &str) -> bool {
    event == "message-seen"
}

// ===========================================================================
// §2.3 — record schema constants
// ===========================================================================

/// Per-chunk sha array cap (§2.3.1 / L2 — re-derived after red-team R1 caught the
/// v1 "56" as a 4096B overflow). Bounds `chunk_sha256s` length to keep the
/// O_APPEND record < 4096B; post-hoc truncation detection is thereby bounded to
/// ~49KB prefixes (the live W8 verify covers the tail). The G1 worst-case-length
/// row is the TEST enforcement; raising this to 56 REDs that row (mutation arm).
pub const CHUNK_SHA_CAP: usize = 48;

/// The production chunk splitter budget (submit.rs `chunk_text(msg, 1024)` —
/// §2.3.1 `chunks` count / §6.3 re-chunking). Pinned here so recovery-read and
/// the writer agree on the SAME boundary the live send used.
pub const CHUNK_BYTES: usize = 1024;

/// O_APPEND atomic-write bound (§4.2, rev C row 22). The only unbounded field is
/// `chunk_sha256s` (capped at [`CHUNK_SHA_CAP`]); the shrink-to-fit belt keeps
/// any future field growth under this too.
pub const MAX_RECORD_BYTES: usize = 4096;

/// ADD-20 (§6.3) — the default `content_preview` byte cap. The preview is redacted
/// then truncated to this many bytes before emission; the §4.2 shrink-to-fit belt
/// may shrink it further (preview yields before any sha) when a worst-case
/// 48-sha line would otherwise overflow [`MAX_RECORD_BYTES`].
pub const PREVIEW_CAP_BYTES: usize = 256;

// ===========================================================================
// §2.2-2.3 — the record (envelope + tagged payload)
// ===========================================================================

/// The CR-1 envelope (§2.2), carried on EVERY record. `session`/`name` are
/// optional but at least one is expected (the FILE key, §4.1, is authoritative).
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// schema_version, always 1 (§2.2 D1: per-record `v`, no first-record bookend
    /// — the engine file is multi-process).
    pub v: u32,
    /// ISO-8601 UTC ms — built from the injected clock via `epoch_ms_to_iso`.
    pub ts: String,
    /// writer id (`std::process::id()`), the multi-writer key (§2.2).
    pub pid: u32,
    /// monotonic PER (pid, file), from 0 (§2.2 — assigned by the writer).
    pub seq: u64,
    /// sessionId when known.
    pub session: Option<String>,
    /// qd session name when known.
    pub name: Option<String>,
    /// RF-6 (R3d) — the EMITTING process's OS start-time (epoch ms), stamped on
    /// every record so the §7 dead-writer rule can tell the genuine original
    /// writer from a recycled pid (a NEW incarnation that happens to hold the old
    /// pid). `None` ⇒ unreadable at emit OR an older/test record (the rule then
    /// falls back to pid-alive only — the v1 fail-safe direction). Additive (§2.2
    /// CR-1: absent, never empty/null), emitted AFTER `seq`, omitted when `None`.
    pub start_ms: Option<i64>,
}

/// The verb that initiated a send (§2.3 `send-initiated.verb`).
pub fn verb_str(is_new_p: bool) -> &'static str {
    if is_new_p {
        "new-p"
    } else {
        "send:pty"
    }
}

/// The `anchor` sub-object of `turn-anchored` (§2.3.3): where OUR turn landed.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub transcript: String,
    pub start_offset: u64,
    pub line_index: u64,
}

/// The engine event payloads (§2.3), EXACT field names/types/optionality.
///
/// Serialized as `{...envelope, "event": "<kebab-kind>", ...payload}` with
/// serde_json `preserve_order` key order pinned by [`build_record_line`]; the
/// `event` tag is the kebab-case kind. `Option` payload fields are OMITTED when
/// `None` (§2.2: "absent, never empty/null").
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// §2.3.1 — emitted BEFORE the first chunk write; the recovery-read anchor.
    SendInitiated {
        send_id: String,
        /// "send:pty" | "new-p".
        verb: String,
        /// "idle" | "busy-queued".
        send_path: String,
        content_sha256: String,
        /// UTF-8 BYTE length of the canonical sent text.
        content_len: u64,
        /// COUNT from chunk_text(msg, 1024) — the production splitter.
        chunks: u32,
        /// PER-CHUNK payload hashes, in order; length = min(chunks, CHUNK_SHA_CAP)
        /// (further shrunk by the §4.2 belt if the line would overflow).
        chunk_sha256s: Vec<String>,
        /// true when `chunk_sha256s` was truncated below `chunks` (§2.3.1 / L2);
        /// omitted when false.
        chunk_sha256s_capped: bool,
        /// resolved transcript path (the recovery-read window key).
        transcript: Option<String>,
        /// pre-send byte length — the recovery-read window key (§2.3.1 / R6/R7).
        transcript_offset: Option<u64>,
        /// ADD-20 (§6.2) — a redacted, ≤256B preview of the sent text
        /// (dispatch's `redact::redact_for_preview`). Additive (CR-1): appended
        /// LAST in the struct AND last in the pinned key order; omitted when None
        /// (an unresolvable text, or shrunk away by the §4.2 belt). NEVER raw
        /// content — API-key-shaped tokens + long runs are scrubbed.
        content_preview: Option<String>,
    },
    /// §2.3.2 — all of this send's per-chunk `mux.send` calls returned Ok.
    ChunksDelivered {
        send_id: String,
        chunks_acked: u32,
        /// "input-sent" | "cli-exit" — the observation channel (named honestly).
        ack_source: String,
    },
    /// §2.3.3 — THE landed signal (terminal).
    TurnAnchored {
        send_id: String,
        content_sha256: String,
        anchor: Anchor,
        /// omitted when false; true only on recovery-read late emission.
        recovered: bool,
        /// present only when recovered — "offset" | "time-window" (§6.1/R6).
        attribution: Option<String>,
    },
    /// §2.3.4 — truncation-landed-as-real-turn (terminal). Fires on sha OR len
    /// disagreement (len = zero-cost belt, rev C row 25).
    TurnAnchoredMismatch {
        send_id: String,
        expected_sha: String,
        actual_sha: String,
        expected_len: u64,
        actual_len: u64,
        recovered: bool,
        attribution: Option<String>,
    },
    /// §2.3.5 — producer-emitted positive timeout (terminal).
    AnchorTimeout { send_id: String, waited_ms: u64 },
    /// §2.3.6 — watch ended without a terminal verdict (terminal).
    /// reason: "watch-interrupted" | "session-died" | "recovery-no-candidate" |
    /// "recovery-unattributable".
    ///
    /// `recovered` / `attribution` are ADDITIVE disclosure flags (R6 seam ruling
    /// 01KX8MDPDX): the recovery-terminus SEARCHED-no-match closer (§6.4) stamps
    /// `recovered: Some(true)` + the search `attribution` ("offset" | "time-window"),
    /// making a landed-but-abandoned send read through D4's disclosed "recovered
    /// (attributed)" category — never a hard "failed". Door/verdict emitters that are
    /// NOT a searched-best-effort recovery (WatchGuard `watch-interrupted`, the legacy
    /// `recovery-unattributable`, `session-died`) pass `None`/`None` (serialized
    /// identically to the pre-R6 bare form — backward-compatible; kind-keyed readers
    /// such as `verdict_from_terminal` are unaffected).
    PendingAbandoned {
        send_id: String,
        reason: String,
        recovered: Option<bool>,
        attribution: Option<String>,
    },
    /// §2.3.7 — weak screen-derived corroborator, advisory (non-terminal).
    ComposerCleared { send_id: String },
    /// §2.3.8 — boot readiness timeout (§5). phase: "pid-file" | "idle". No
    /// send_id (terminal for the BOOT, not a send).
    PrimingReadinessTimeout { waited_ms: u64, phase: String },
    /// §2.3.9 — observed status change. source is a v1 constant. No send_id.
    StatusTransition {
        status: String,
        /// "status-file-poll" (v1 constant, documented in-schema).
        source: String,
    },
    /// §2.3.10 — rotation marker, envelope only (§4.3).
    EventsTruncated,
    /// §X.3.2 (3-phase delivery) — relay on-queued ack; NON-terminal (the relay
    /// analog of `chunks-delivered`). Emitted SENDER-side into the TARGET's log
    /// immediately after the `send:relay` POST returns the server-minted
    /// `message_id`. `send_id = message_id`. `content_sha256` is carried for
    /// diagnostics/parity; on-queued resolves by `send_id` ONLY (§X.4).
    RelayDelivered {
        send_id: String,
        content_sha256: String,
    },
    /// C5/C3 (3-phase delivery, DAEMON lanes) — the daemon-arm DELIVERED ack;
    /// NON-terminal (the codex / `acp/*` / pi analog of `relay-delivered`). Emitted
    /// SENDER-side into the TARGET's log the moment the resident ACCEPTS the turn
    /// (the `inject` ACK returns the resident-minted turn id — send_relay.rs
    /// `run_codex_send`/`run_acp_send`/`run_pi_send`). Its C3 delivered-STRENGTH is
    /// "turn-accepted" — the resident took the prompt as a turn — deliberately
    /// distinct from relay's queue-receipt and pty's chunks-acked, and NEVER
    /// conflated with landed: only the TERMINAL (`message-seen` for the pi/codex
    /// rollout observer; the StopReason-mapped terminal for ACP) says landed.
    /// `send_id` = the resident turn id; `content_sha256` carries the sent bytes'
    /// hash for correlation (the same key the observer/consumer matches on). Never
    /// carries prose. Additive (spec interpretation (ii) / gate item 1).
    TurnAccepted {
        send_id: String,
        content_sha256: String,
    },
    /// §X.3.4 (3-phase delivery) — on-received, TERMINAL (success). The uniform
    /// "the recipient pulled it into working context" signal for BOTH transports
    /// (relay: a recipient-side transcript observer; pty: the W8 ungate). Deliberately
    /// a NEW kind, NOT `turn-anchored`, so a consumer's on-received `reason=Seen` gate can
    /// never be tripped by a `--wait`/W8 `turn-anchored` anchor. `send_id` is
    /// MANDATORY (a consumer drops a terminal without it). `content_sha256`: pty hashes the
    /// raw message; relay hashes the extracted inner body (recipient-side ADVISORY —
    /// terminals resolve by `send_id` only, §X.4). Never carries prose (§X.7).
    MessageSeen {
        send_id: String,
        content_sha256: String,
    },
    /// §X.3.5 (3-phase delivery) — on-received failure, TERMINAL. Fires ONLY on a
    /// genuine recipient-gone (the `session-closed` bookend + transcript-absence of
    /// the `message_id`), NEVER on latency (an un-pulled-but-alive message stays
    /// PENDING). `reason` ∈ "recipient-gone" | "transport-error" (extend additively).
    /// `send_id` MANDATORY.
    SeenFailed { send_id: String, reason: String },
    /// C1 (delivery contract, spec §C1) — a DOOR failure BEFORE wire activity,
    /// TERMINAL (failure). Emitted when a send against a RESOLVED target session
    /// fails before it reaches the wire (relay `no_relay_exit`; the daemon arms'
    /// dead-adapter / refused-ws / session=None surfaces) — the "no silent failures
    /// at the door" record that replaces stderr-only exits.
    ///
    /// `send_id` is OPTIONAL (spec-verbatim `send-failed { send_id?, reason }`): a
    /// relay `send_id` is the SERVER-minted `message_id` returned by the POST, which
    /// has NOT happened at a pre-wire door failure — omitted, never client-invented.
    /// A door failure is SYNCHRONOUS: the invoker learns it from the exit path it
    /// already holds; joinability is not required pre-wire (every post-wire terminal
    /// carries a mandatory `send_id`). `content_sha256` is ALWAYS carried (the door
    /// has the message bytes in hand) for correlation. `reason` is a short token
    /// (e.g. "no-relay"); extend additively. NEVER carries prose.
    SendFailed {
        send_id: Option<String>,
        content_sha256: String,
        reason: String,
    },
    /// R3d (recovery-ladder forensics) — a recovery RUNG was ENTERED for a session.
    /// NON-terminal (recovery telemetry, not a send terminal); carries NO send_id
    /// (recovery is session-scoped). `rung` is the lowercase `recovery::Rung`
    /// token ("pidfd-signal" | "control-wake" | "pty-inject" | "respawn"). With
    /// `rung-succeeded` / `rung-timeout` / `recovery-crit`, the event log ALONE
    /// reconstructs which rungs a recovery episode entered/succeeded/timed-out/CRITed
    /// (`replay_recovery_episode`).
    RungEntered { session_id: String, rung: String },
    /// R3d — a recovery rung SUCCEEDED (the session recovered at this rung).
    /// NON-terminal, no send_id.
    RungSucceeded { session_id: String, rung: String },
    /// R3d — a recovery rung TIMED OUT (the rung's deadline elapsed with no
    /// recovery; the ladder escalates or, at Rung 4, records a confirmed failure).
    /// NON-terminal, no send_id.
    RungTimeout {
        session_id: String,
        rung: String,
        waited_ms: u64,
    },
    /// R3d — the recovery ladder reached CRIT (≥`CRIT_CONSECUTIVE_FAILURES`
    /// confirmed failures, or a D-state target): terminal for the EPISODE, operator
    /// alert, no further automation. NON-terminal in the SEND sense (no send_id);
    /// `consecutive_failures` is the strike count at CRIT.
    RecoveryCrit {
        session_id: String,
        consecutive_failures: u32,
    },
}

impl Payload {
    /// The kebab-case event tag (the serde `event` value, §2.2).
    pub fn event_name(&self) -> &'static str {
        match self {
            Payload::SendInitiated { .. } => "send-initiated",
            Payload::ChunksDelivered { .. } => "chunks-delivered",
            Payload::TurnAnchored { .. } => "turn-anchored",
            Payload::TurnAnchoredMismatch { .. } => "turn-anchored-mismatch",
            Payload::AnchorTimeout { .. } => "anchor-timeout",
            Payload::PendingAbandoned { .. } => "pending-abandoned",
            Payload::ComposerCleared { .. } => "composer-cleared",
            Payload::PrimingReadinessTimeout { .. } => "priming-readiness-timeout",
            Payload::StatusTransition { .. } => "status-transition",
            Payload::EventsTruncated => "events-truncated",
            Payload::RelayDelivered { .. } => "relay-delivered",
            Payload::TurnAccepted { .. } => "turn-accepted",
            Payload::MessageSeen { .. } => "message-seen",
            Payload::SeenFailed { .. } => "seen-failed",
            Payload::SendFailed { .. } => "send-failed",
            Payload::RungEntered { .. } => "rung-entered",
            Payload::RungSucceeded { .. } => "rung-succeeded",
            Payload::RungTimeout { .. } => "rung-timeout",
            Payload::RecoveryCrit { .. } => "recovery-crit",
        }
    }

    /// True for the terminal-CLASS records that survive the rotation reserve band
    /// (§4.3): the [`TERMINAL_EVENTS`] + `priming-readiness-timeout` +
    /// `events-truncated`. `pub` so the dispatch-side writer (`emit`) can gate the
    /// reserve band cross-crate.
    pub fn is_terminal_class(&self) -> bool {
        is_terminal(self.event_name())
            || matches!(
                self,
                Payload::PrimingReadinessTimeout { .. } | Payload::EventsTruncated
            )
    }

    /// Insert this payload's fields into `obj` AFTER the `event` tag, in §2.3 key
    /// order. `Option` fields omitted when `None`; `*_capped`/`recovered` bools
    /// omitted when false (§2.3). `sha_cap` bounds `chunk_sha256s` length; the
    /// returned bool reports whether the array was shrunk below `chunks` here.
    fn insert_fields(&self, obj: &mut Map<String, Value>, sha_cap: usize) {
        match self {
            Payload::SendInitiated {
                send_id,
                verb,
                send_path,
                content_sha256,
                content_len,
                chunks,
                chunk_sha256s,
                chunk_sha256s_capped,
                transcript,
                transcript_offset,
                content_preview,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert("verb".into(), Value::String(verb.clone()));
                obj.insert("send_path".into(), Value::String(send_path.clone()));
                obj.insert(
                    "content_sha256".into(),
                    Value::String(content_sha256.clone()),
                );
                obj.insert("content_len".into(), Value::from(*content_len));
                obj.insert("chunks".into(), Value::from(*chunks));
                // Apply the cap (§2.3.1): length = min(provided, sha_cap). The
                // shrink-to-fit belt (§4.2) calls back in with a SMALLER cap.
                let kept = chunk_sha256s.len().min(sha_cap);
                let capped = *chunk_sha256s_capped || kept < chunk_sha256s.len();
                let arr: Vec<Value> = chunk_sha256s[..kept]
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect();
                obj.insert("chunk_sha256s".into(), Value::Array(arr));
                if capped {
                    obj.insert("chunk_sha256s_capped".into(), Value::Bool(true));
                }
                insert_opt_str(obj, "transcript", transcript);
                if let Some(o) = transcript_offset {
                    obj.insert("transcript_offset".into(), Value::from(*o));
                }
                // ADD-20 (§6.2): content_preview LAST. Omitted when None (the §4.2
                // belt drives it to None on overflow; see `fit_line`).
                insert_opt_str(obj, "content_preview", content_preview);
            }
            Payload::ChunksDelivered {
                send_id,
                chunks_acked,
                ack_source,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert("chunks_acked".into(), Value::from(*chunks_acked));
                obj.insert("ack_source".into(), Value::String(ack_source.clone()));
            }
            Payload::TurnAnchored {
                send_id,
                content_sha256,
                anchor,
                recovered,
                attribution,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert(
                    "content_sha256".into(),
                    Value::String(content_sha256.clone()),
                );
                obj.insert("anchor".into(), anchor_value(anchor));
                if *recovered {
                    obj.insert("recovered".into(), Value::Bool(true));
                }
                insert_opt_str(obj, "attribution", attribution);
            }
            Payload::TurnAnchoredMismatch {
                send_id,
                expected_sha,
                actual_sha,
                expected_len,
                actual_len,
                recovered,
                attribution,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert("expected_sha".into(), Value::String(expected_sha.clone()));
                obj.insert("actual_sha".into(), Value::String(actual_sha.clone()));
                obj.insert("expected_len".into(), Value::from(*expected_len));
                obj.insert("actual_len".into(), Value::from(*actual_len));
                if *recovered {
                    obj.insert("recovered".into(), Value::Bool(true));
                }
                insert_opt_str(obj, "attribution", attribution);
            }
            Payload::AnchorTimeout { send_id, waited_ms } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert("waited_ms".into(), Value::from(*waited_ms));
            }
            Payload::PendingAbandoned {
                send_id,
                reason,
                recovered,
                attribution,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert("reason".into(), Value::String(reason.clone()));
                // Additive R6 disclosure flags — mirror TurnAnchored/TurnAnchoredMismatch:
                // emit "recovered":true only when set, attribution only when Some. A bare
                // watch-interrupted / session-died / unattributable terminal serializes
                // exactly as before (no new keys).
                if *recovered == Some(true) {
                    obj.insert("recovered".into(), Value::Bool(true));
                }
                insert_opt_str(obj, "attribution", attribution);
            }
            Payload::ComposerCleared { send_id } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
            }
            Payload::PrimingReadinessTimeout { waited_ms, phase } => {
                obj.insert("waited_ms".into(), Value::from(*waited_ms));
                obj.insert("phase".into(), Value::String(phase.clone()));
            }
            Payload::StatusTransition { status, source } => {
                obj.insert("status".into(), Value::String(status.clone()));
                obj.insert("source".into(), Value::String(source.clone()));
            }
            Payload::EventsTruncated => {}
            // §X.3.2/§X.3.4/§X.3.5 — the 3-phase delivery kinds. Key order:
            // send_id first, then content_sha256 (RelayDelivered/MessageSeen) or
            // reason (SeenFailed). All required (no omit-when-None here).
            Payload::RelayDelivered {
                send_id,
                content_sha256,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert(
                    "content_sha256".into(),
                    Value::String(content_sha256.clone()),
                );
            }
            // C5/C3 daemon-lane delivered ack. Same key order as RelayDelivered
            // (send_id, then content_sha256); NON-terminal, both required.
            Payload::TurnAccepted {
                send_id,
                content_sha256,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert(
                    "content_sha256".into(),
                    Value::String(content_sha256.clone()),
                );
            }
            Payload::MessageSeen {
                send_id,
                content_sha256,
            } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert(
                    "content_sha256".into(),
                    Value::String(content_sha256.clone()),
                );
            }
            Payload::SeenFailed { send_id, reason } => {
                obj.insert("send_id".into(), Value::String(send_id.clone()));
                obj.insert("reason".into(), Value::String(reason.clone()));
            }
            // §C1 door failure. Key order: send_id? (OMITTED when None, §2.2 absent-
            // never-null), then content_sha256, then reason. `send_id` is optional at
            // this door (no server-minted id pre-wire); content_sha256 is always
            // carried for correlation.
            Payload::SendFailed {
                send_id,
                content_sha256,
                reason,
            } => {
                insert_opt_str(obj, "send_id", send_id);
                obj.insert(
                    "content_sha256".into(),
                    Value::String(content_sha256.clone()),
                );
                obj.insert("reason".into(), Value::String(reason.clone()));
            }
            // R3d recovery-ladder forensics. Key order: session_id first, then the
            // kind-specific fields. No send_id (recovery is session-scoped).
            Payload::RungEntered { session_id, rung }
            | Payload::RungSucceeded { session_id, rung } => {
                obj.insert("session_id".into(), Value::String(session_id.clone()));
                obj.insert("rung".into(), Value::String(rung.clone()));
            }
            Payload::RungTimeout {
                session_id,
                rung,
                waited_ms,
            } => {
                obj.insert("session_id".into(), Value::String(session_id.clone()));
                obj.insert("rung".into(), Value::String(rung.clone()));
                obj.insert("waited_ms".into(), Value::from(*waited_ms));
            }
            Payload::RecoveryCrit {
                session_id,
                consecutive_failures,
            } => {
                obj.insert("session_id".into(), Value::String(session_id.clone()));
                obj.insert(
                    "consecutive_failures".into(),
                    Value::from(*consecutive_failures),
                );
            }
        }
    }
}

fn anchor_value(a: &Anchor) -> Value {
    let mut obj = Map::new();
    obj.insert("transcript".into(), Value::String(a.transcript.clone()));
    obj.insert("start_offset".into(), Value::from(a.start_offset));
    obj.insert("line_index".into(), Value::from(a.line_index));
    Value::Object(obj)
}

pub(crate) fn insert_opt_str(obj: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(s) = v.as_ref().filter(|s| !s.is_empty()) {
        obj.insert(key.to_string(), Value::String(s.clone()));
    }
}

/// Build the single-line JSON record (§2.2 key order: envelope first, then
/// `event`, then payload fields). `sha_cap` bounds a `send-initiated` payload's
/// `chunk_sha256s` (the §4.2 shrink belt drives it down on overflow).
///
/// `pub` so the dispatch-side writer (`emit`/`fit_line`) can build the byte-exact
/// line cross-crate. Byte-exactness DEPENDS on serde_json `preserve_order` — see
/// the crate-level docs + Cargo.toml.
pub fn build_record_line(env: &Envelope, payload: &Payload, sha_cap: usize) -> String {
    let mut obj = Map::new();
    // Envelope, pinned order: v, ts, pid, seq, session?, name?.
    obj.insert("v".into(), Value::from(env.v));
    obj.insert("ts".into(), Value::String(env.ts.clone()));
    obj.insert("pid".into(), Value::from(env.pid));
    obj.insert("seq".into(), Value::from(env.seq));
    // RF-6 start_ms: emitted right after seq, omitted when None (§2.2 additive).
    if let Some(sm) = env.start_ms {
        obj.insert("start_ms".into(), Value::from(sm));
    }
    insert_opt_str(&mut obj, "session", &env.session);
    insert_opt_str(&mut obj, "name", &env.name);
    // The serde tag.
    obj.insert(
        "event".into(),
        Value::String(payload.event_name().to_string()),
    );
    // Payload fields, §2.3 order.
    payload.insert_fields(&mut obj, sha_cap);
    Value::Object(obj).to_string()
}
