//! ACK-2 engine event emitter core (ack2-spec §1).
//!
//! One library module owns the engine event surface: the CR-1 record schema
//! (§2), the CR-2 terminal set (§3), the multi-process file writer + torn-
//! tolerant reader (§4), the `send_id` mint (§2.1), recovery-read (§6), the
//! reader-side dead-writer rule (§7), and the `await_received` consumer helper
//! (§8). This is M1 (schema/writer/reader/mint/terminal-set) + M4 (recovery-
//! read/dead-writer/await_received); the verb-wiring (M2/M3, §5/§9) lives in the
//! verbs/ layer and is NOT in this module.
//!
//! # House seams (ack2-spec §1)
//!
//! - fs via std (the writer/reader take resolved paths, mirror telemetry.rs);
//! - the clock is the injected [`crate::effects::Clock`] (L9a — no raw `now`);
//! - every poll loop is sleep/clock-seamed (the [`AwaitDeps`] / [`RecoveryDeps`]
//!   traits, mirroring [`crate::sendpty::WaitDeps`]) so unit tests run instantly.
//!
//! # Best-effort, non-fatal (ack2-spec §4.2)
//!
//! Emission is BEST-EFFORT everywhere: an emit failure warns to stderr and never
//! changes a verb's exit code (the A6 telemetry contract). All emit fns return
//! `Result<(), String>` the caller logs-and-ignores.
//!
//! # Privacy (ack2-spec §9 privacy row, VERBATIM — do not weaken)
//!
//! Records carry `content_sha256` + `content_len` ONLY — never raw message text.
//! The sha is a confirmation oracle (a guessable/structured message can be
//! confirmed by hashing candidates); it is kept because truncation detection
//! (`turn-anchored-mismatch`) needs it. `chunk_sha256s` are shas of content
//! SUBSTRINGS — a finer-grained oracle of the same class. This is
//! pseudonymization for high-entropy text, not a one-way veil. G10 greps every
//! emitted record for raw payloads → must be absent.
//!
//! ## CORRECTION (W1.3 / SPEC-v2 §4.C, G6) — reconciling the VERBATIM invariant
//!
//! The invariant above is contradicted by SHIPPED behavior and must not mislead
//! an external reader: `send-initiated` ALSO carries `content_preview` (see that
//! field + `redact.rs`) — redacted-but-**readable** message prose (secrets-
//! scrubbed, ≤256 B), NOT sha+len only. So the delivery log DOES carry readable
//! prose. The redactor is unchanged (W4); the consumer-side fix is the consumer's §4.C
//! read-allowlist, which EXCLUDES `content_preview`. See `doc/EVENT-CONTRACT.md`.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use crate::effects::{is_pid_alive, Clock};
use crate::render::epoch_ms_to_iso;
use crate::sendpty::{parse_jsonl_slice, user_record_text, JsonlRecord};
use crate::submit::chunk_text;

// The delivery-event VOCABULARY (the producer surface: the `Payload` enum,
// `Envelope`/`Anchor`, the terminal set, the record-schema constants, and the
// byte-exact wire serializer) was extracted to the leaf crate
// `quorum-delivery-events` so `qrmux` (attended-UX M1/M3 emitters) can emit the
// same vocab without a `qrmux → dispatch` dependency cycle. The writer
// (`EventWriter`/`emit`), reader (`parse_events`/`read_merged`), and ALL resolver
// logic stay HERE and import the vocab from the leaf crate.
//
// Re-exported so every existing `dispatch::events::Foo` / `crate::events::Foo`
// call site (across dispatch src + tests, and downstream) compiles and behaves
// BYTE-IDENTICALLY with zero edits. `build_record_line` (used internally by the
// writer's `emit`/`fit_line`) is re-exported too.
pub use quorum_delivery_events::{
    build_record_line, is_success_terminal, is_terminal, sha256_hex, verb_str, Anchor, Envelope,
    Payload, CHUNK_BYTES, CHUNK_SHA_CAP, MAX_RECORD_BYTES, PREVIEW_CAP_BYTES, TERMINAL_EVENTS,
};

// ===========================================================================
// §2.1 — send_id mint
// ===========================================================================

/// Process-wide `n` counter for [`mint_send_id`] (§2.1): unique WITHIN a process.
/// Uniqueness across processes comes from `pid`; across pid-reuse from `epoch_ms`.
static SEND_ID_N: AtomicU64 = AtomicU64::new(0);

/// Mint a `send_id` (§2.1): `"{pid}-{epoch_ms}-{n}"`.
///
/// - `pid` = `std::process::id()` (multi-writer key, also carried in the
///   envelope; the dead-writer rule reads the ENVELOPE pid, never parses this);
/// - `epoch_ms` = mint-time clock ms (injected [`Clock`], L9a);
/// - `n` = process-wide [`AtomicU64`] counter.
///
/// OPAQUE to consumers — equality only; nobody parses it (§2.1 contract).
pub fn mint_send_id(clock: &dyn Clock) -> String {
    let pid = std::process::id();
    let epoch_ms = clock.now_ms();
    let n = SEND_ID_N.fetch_add(1, Ordering::SeqCst);
    format!("{pid}-{epoch_ms}-{n}")
}

// ===========================================================================
// §3 / §2.3 — terminal set + record-schema VOCAB
// ===========================================================================
//
// `sha256_hex`, `TERMINAL_EVENTS`, `is_terminal`, and the record-schema
// constants `CHUNK_SHA_CAP` / `CHUNK_BYTES` / `MAX_RECORD_BYTES` /
// `PREVIEW_CAP_BYTES` moved to the `quorum-delivery-events` leaf crate and are
// re-exported from the module head above (byte-identical behavior). The
// WRITER/READER-policy rotation constants below stay in dispatch — they are not
// part of the moved wire/producer surface (nothing in the moved serialize code
// references them; `emit`'s rotation reserve band does).

/// Rotation cap (§4.3): files above this take terminal-class records only.
pub const EVENTS_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Rotation reserve (§4.3): the band above the cap where terminal-class records
/// are still accepted (so a verdict can always land).
pub const EVENTS_RESERVE_BYTES: u64 = 64 * 1024;

/// Reader-side dead-writer age threshold (§7, `T_anchor_idle`): a dangling
/// `send-initiated` whose writer pid is dead AND older than this is dead-
/// dangling → recovery-read.
pub const T_ANCHOR_IDLE_MS: i64 = 30_000;

// ===========================================================================
// §2.2-2.3 — the record (envelope + tagged payload)
// ===========================================================================
//
// `Envelope`, `Anchor`, the `Payload` enum (all 19 variants) + its
// `event_name`/`is_terminal_class`/`insert_fields` methods, plus `anchor_value`,
// `insert_opt_str`, and `build_record_line` moved to the
// `quorum-delivery-events` leaf crate (re-exported above). They are PURE (no
// `crate::` references), so the move is byte-preserving; the leaf crate's
// `golden_wire` test pins the wire bytes.

/// The emitting process's OWN OS start-time (epoch ms), memoized process-globally
/// (RF-6 / R3d): one [`crate::effects::proc_start_ms`] read per process lifetime
/// (a `ps -o etime=` spawn is too costly per-record), stamped on every envelope as
/// `start_ms`. The process's start-time is constant, so a single read is exact for
/// the whole process. A read failure caches `None` (the dead-writer rule then keeps
/// its v1 pid-alive-only behavior — never a spurious trigger).
fn self_start_ms() -> Option<i64> {
    static SELF_START: OnceLock<Option<i64>> = OnceLock::new();
    *SELF_START.get_or_init(|| crate::effects::proc_start_ms(std::process::id() as i32))
}

// (`verb_str`, `Anchor`, `Payload` + methods, `anchor_value`, `insert_opt_str`,
// `build_record_line` moved to the `quorum-delivery-events` leaf crate — re-exported
// from the module head above.)

// ===========================================================================
// §4.1 — path + key (state tier, QD_HOME-honoring, ADD-14-clean)
// ===========================================================================

/// Resolve the engine events file for a session (§4.1). `state_dir` is the
/// caller-resolved `QdPaths.state_dir` (QD_HOME-honoring via the injected Env
/// upstream — never literal /tmp, ADD-14). `key` is the sessionId when known,
/// else `byname-<name>` (the §10 D5 fallback the caller composes via
/// [`byname_key`]). `byname-` cannot collide with a sessionId (uuid alphabet).
pub fn events_path(state_dir: &Path, key: &str) -> PathBuf {
    events_dir(state_dir).join(format!("{key}.events.jsonl"))
}

/// The directory holding every session's `*.events.jsonl` file (§4.1). Single
/// source of truth for the `sessions/` subdir, so the recovery sweep
/// ([`crate::events`] consumers) enumerates the SAME place [`events_path`] writes.
pub fn events_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

/// The §4.1 D5 byname fallback key: `byname-<name>` for when sessionId is
/// unresolvable at emission time (new -p before the registry row, or a failed
/// boot's `priming-readiness-timeout`).
pub fn byname_key(name: &str) -> String {
    format!("byname-{name}")
}

/// The two files a reader given `(session_id?, name?)` must MERGE (§4.1): the
/// sessionId file and the byname file. Either/both may be absent.
pub fn reader_paths(
    state_dir: &Path,
    session_id: Option<&str>,
    name: Option<&str>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        out.push(events_path(state_dir, sid));
    }
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        out.push(events_path(state_dir, &byname_key(n)));
    }
    out
}

/// The reader keying context (§4.1): the state dir + the (sessionId?, name?) that
/// select + merge the event file(s). Bundled so the recovery/await helpers don't
/// thread three positional args each.
#[derive(Clone, Copy)]
pub struct ReaderCtx<'a> {
    pub state_dir: &'a Path,
    pub session_id: Option<&'a str>,
    pub name: Option<&'a str>,
}

impl<'a> ReaderCtx<'a> {
    /// Read + merge this context's event file(s) (§4.1 / §4.4).
    pub fn read(&self) -> ReadResult {
        read_merged(self.state_dir, self.session_id, self.name)
    }
}

// ===========================================================================
// §2.2 / §4.2 — per-(pid,file) seq registry
// ===========================================================================

/// Process-global per-(pid, file) seq counter (§2.2): two writers in ONE process
/// must not fork a pid's stream, so the seq is keyed on the resolved file PATH
/// (one pid per process). Returns the next seq and advances it.
fn next_seq(path: &Path) -> u64 {
    static SEQ: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    let map = SEQ.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    let slot = guard.entry(path.to_path_buf()).or_insert(0);
    let seq = *slot;
    *slot += 1;
    seq
}

// ===========================================================================
// §4.2-4.3 — the writer
// ===========================================================================

/// An engine event writer bound to a resolved file path (§4.1). Holds the path;
/// the seq comes from the process-global [`next_seq`] registry, the pid from the
/// process. One writer per (resolved) file; all emission is best-effort
/// non-fatal (§4.2).
pub struct EventWriter {
    path: PathBuf,
    /// session/name carried onto every envelope this writer emits (§2.2).
    session: Option<String>,
    name: Option<String>,
}

impl EventWriter {
    /// Bind a writer to `path` (use [`events_path`] to resolve it) with the
    /// `session`/`name` it stamps on each envelope.
    pub fn new(path: PathBuf, session: Option<String>, name: Option<String>) -> Self {
        EventWriter {
            path,
            session,
            name,
        }
    }

    /// Bind a writer for `key` under `state_dir` (the common case).
    pub fn for_key(
        state_dir: &Path,
        key: &str,
        session: Option<String>,
        name: Option<String>,
    ) -> Self {
        Self::new(events_path(state_dir, key), session, name)
    }

    /// The bound file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Emit one record (§4.2-4.3), best-effort non-fatal. Assigns the seq + ts
    /// (from `clock`), applies rotation (§4.3) and the shrink-to-fit overflow
    /// belt (§4.2), then does ONE `O_APPEND` `write_all` of `line + "\n"`.
    ///
    /// `Err` carries a human reason for the caller to WARN about; the caller MUST
    /// NOT change its exit code on failure (§4.2 — best-effort durable).
    pub fn emit(&self, clock: &dyn Clock, payload: &Payload) -> Result<(), String> {
        // §4.3 rotation: stat first; decide whether this record may be written.
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let cap = EVENTS_MAX_BYTES;
        let reserve = EVENTS_RESERVE_BYTES;
        let mut rotation_marker = false;
        if size > cap + reserve {
            // Hard floor: drop everything (§4.3).
            return Err(format!(
                "events file {} over hard floor ({size}B > {}B) — record dropped",
                self.path.display(),
                cap + reserve
            ));
        }
        if size > cap {
            // Reserve band: terminal-class only; others dropped with a WARN.
            if !payload.is_terminal_class() {
                return Err(format!(
                    "events file {} in reserve band ({size}B > {cap}B) — non-terminal {} dropped",
                    self.path.display(),
                    payload.event_name()
                ));
            }
            // Marker ONCE per file (§4.3, lead review fix): only when the file
            // does not already carry one — a per-write re-emit would consume the
            // reserve with markers. Band writes are rare (file > 5MB), so the
            // read-and-check is acceptable here. The cross-PROCESS check-then-
            // append race can still duplicate the marker — that residual is the
            // §4.3-tolerated class (readers idempotent); systematic per-write
            // duplication is not.
            rotation_marker = !std::fs::read_to_string(&self.path)
                .map(|t| t.contains("\"event\":\"events-truncated\""))
                .unwrap_or(false);
        }

        // Emit the rotation marker FIRST (lead review fix: it takes the EARLIER
        // seq, so per-pid seq order matches file order — assigning the record's
        // seq before writing the marker would invert the pair). Best-effort; a
        // failed marker write does not block the record.
        if rotation_marker {
            let marker_env = Envelope {
                v: 1,
                ts: epoch_ms_to_iso(clock.now_ms()),
                pid: std::process::id(),
                seq: next_seq(&self.path),
                session: self.session.clone(),
                name: self.name.clone(),
                start_ms: self_start_ms(),
            };
            let marker = build_record_line(&marker_env, &Payload::EventsTruncated, CHUNK_SHA_CAP);
            let _ = append_record(&self.path, &marker);
        }

        // Assign seq + build the line (with the §4.2 shrink-to-fit belt).
        let env = Envelope {
            v: 1,
            ts: epoch_ms_to_iso(clock.now_ms()),
            pid: std::process::id(),
            seq: next_seq(&self.path),
            session: self.session.clone(),
            name: self.name.clone(),
            start_ms: self_start_ms(),
        };
        let line = self.fit_line(&env, payload);
        append_record(&self.path, &line)
    }

    /// Build the line for `env`+`payload`, applying the §4.2 SHRINK-TO-FIT belt.
    ///
    /// ADD-20 (§6.3) NEW SHRINK ORDER for a `send-initiated` record that would
    /// exceed [`MAX_RECORD_BYTES`]:
    ///   1. truncate `content_preview` FIRST — progressively, down to None
    ///      (the field is then omitted entirely);
    ///   2. ONLY THEN drop trailing `chunk_sha256s` entries (setting
    ///      `chunk_sha256s_capped`).
    ///
    /// `content_sha256`, `content_len` and the surviving shas are NEVER sacrificed
    /// before the preview is FULLY gone — preview is debuggability, shas are
    /// functional (mismatch / recovery machinery). The record is NEVER skipped
    /// (red-team R1 — a skipped `send-initiated` makes the send invisible to
    /// recovery-read).
    fn fit_line(&self, env: &Envelope, payload: &Payload) -> String {
        // Phase 1 — preview shrink (send-initiated with a content_preview only).
        // Find the LARGEST preview byte-cap (down to 0 = field omitted) that, at
        // the FULL sha cap, keeps the line under the bound — preview yields before
        // any sha (§6.3), but only by as much as it must (the preview keeps as
        // close to the margin as a char boundary allows). For every other payload
        // (and a send-initiated without a preview) this is a no-op.
        let mut payload_owned: Option<Payload> = None;
        if let Payload::SendInitiated {
            content_preview: Some(preview),
            ..
        } = payload
        {
            let fits = |cap: usize| -> bool {
                let candidate = if cap == 0 {
                    None
                } else {
                    Some(truncate_preview_bytes(preview, cap))
                };
                let p = with_preview(payload, candidate);
                build_record_line(env, &p, CHUNK_SHA_CAP).len() < MAX_RECORD_BYTES
            };
            if !fits(preview.len()) {
                // Binary search the maximal fitting cap in [0, preview.len()].
                // `fits` is monotone (a smaller preview never makes the line
                // longer). lo always fits (0 = omitted is the floor); hi never
                // fits (the full length overflowed). Converge to lo = max-fit.
                let mut lo = 0usize; // fits
                let mut hi = preview.len(); // does not fit
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if fits(mid) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let chosen = if lo == 0 {
                    // Even an empty preview overflows → omit it; Phase 2 drops shas.
                    None
                } else {
                    Some(truncate_preview_bytes(preview, lo))
                };
                payload_owned = Some(with_preview(payload, chosen));
            }
        }
        let payload: &Payload = payload_owned.as_ref().unwrap_or(payload);

        // Phase 2 — sha shrink (the legacy belt, unchanged). Runs ONLY if the line
        // still overflows after the preview is fully gone. Drops trailing
        // chunk_sha256s entries until the record fits.
        let mut cap = CHUNK_SHA_CAP;
        let mut line = build_record_line(env, payload, cap);
        while line.len() >= MAX_RECORD_BYTES && cap > 0 {
            cap -= 1;
            line = build_record_line(env, payload, cap);
        }
        debug_assert!(
            line.len() < MAX_RECORD_BYTES || cap == 0,
            "record exceeded {MAX_RECORD_BYTES}B after shrink-to-fit: {}B",
            line.len() + 1
        );
        line
    }
}

/// Clone a `SendInitiated` payload with its `content_preview` replaced (ADD-20
/// §6.3 belt helper). For any other payload this is a plain clone (the belt only
/// calls it on send-initiated).
fn with_preview(payload: &Payload, preview: Option<String>) -> Payload {
    match payload {
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
            content_preview: _,
        } => Payload::SendInitiated {
            send_id: send_id.clone(),
            verb: verb.clone(),
            send_path: send_path.clone(),
            content_sha256: content_sha256.clone(),
            content_len: *content_len,
            chunks: *chunks,
            chunk_sha256s: chunk_sha256s.clone(),
            chunk_sha256s_capped: *chunk_sha256s_capped,
            transcript: transcript.clone(),
            transcript_offset: *transcript_offset,
            content_preview: preview,
        },
        other => other.clone(),
    }
}

/// Truncate a preview string to at most `cap` bytes on a char boundary (ADD-20
/// §6.3 belt helper — no marker; the belt already signals shrink by the shorter
/// body / omitted field).
fn truncate_preview_bytes(preview: &str, cap: usize) -> String {
    if preview.len() <= cap {
        return preview.to_string();
    }
    let mut cut = cap;
    while cut > 0 && !preview.is_char_boundary(cut) {
        cut -= 1;
    }
    preview[..cut].to_string()
}

/// Append one `line + "\n"` to `path` via `O_APPEND | O_CREAT` mode 0600, parent
/// `create_dir_all` (§4.2; house pattern telemetry.rs:184 `append_line`). ONE
/// `write_all` per record (the ≤4096B atomic-append contract).
fn append_record(path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create sessions dir: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    f.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("write failed: {e}"))
}

// ===========================================================================
// R3d — recovery-ladder forensics (emit + replay)
// ===========================================================================

/// One transition of a recovery episode, the typed surface over the four R3d
/// ladder payloads. The coordinator/ladder emits these as the episode runs; a
/// reader reconstructs the episode from the log ALONE ([`replay_recovery_episode`])
/// — the "forensically reconstructable" property (R3d / R1 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LadderEvent {
    /// A rung was entered (`rung` = the [`crate::recovery::Rung::as_str`] token).
    RungEntered { session_id: String, rung: String },
    /// A rung succeeded (the session recovered at it).
    RungSucceeded { session_id: String, rung: String },
    /// A rung timed out (deadline elapsed with no recovery).
    RungTimeout {
        session_id: String,
        rung: String,
        waited_ms: u64,
    },
    /// The episode reached CRIT (terminal; operator alert).
    Crit {
        session_id: String,
        consecutive_failures: u32,
    },
}

impl LadderEvent {
    /// The wire [`Payload`] for this event.
    pub fn payload(&self) -> Payload {
        match self {
            LadderEvent::RungEntered { session_id, rung } => Payload::RungEntered {
                session_id: session_id.clone(),
                rung: rung.clone(),
            },
            LadderEvent::RungSucceeded { session_id, rung } => Payload::RungSucceeded {
                session_id: session_id.clone(),
                rung: rung.clone(),
            },
            LadderEvent::RungTimeout {
                session_id,
                rung,
                waited_ms,
            } => Payload::RungTimeout {
                session_id: session_id.clone(),
                rung: rung.clone(),
                waited_ms: *waited_ms,
            },
            LadderEvent::Crit {
                session_id,
                consecutive_failures,
            } => Payload::RecoveryCrit {
                session_id: session_id.clone(),
                consecutive_failures: *consecutive_failures,
            },
        }
    }
}

/// Emit one recovery-ladder forensic event, best-effort non-fatal (§4.2 — an emit
/// failure NEVER changes recovery behavior; the ladder is the source of truth, the
/// log is forensics). Append-only `O_APPEND`, ≤[`MAX_RECORD_BYTES`], via the same
/// rotation-aware [`EventWriter::emit`] path every record uses.
pub fn emit_ladder_event(
    writer: &EventWriter,
    clock: &dyn Clock,
    event: &LadderEvent,
) -> Result<(), String> {
    writer.emit(clock, &event.payload())
}

/// Reconstruct a recovery episode from the event log ALONE (R3d forensics). Reads
/// the four recovery-ladder kinds in FILE ORDER (the forensic cross-pid order, §4.4)
/// and returns the [`LadderEvent`] sequence — proving an episode (which rungs
/// entered / succeeded / timed-out / CRITed) is replayable WITHOUT any in-memory
/// coordinator state. Non-recovery records are ignored.
pub fn replay_recovery_episode(records: &[EventRecord]) -> Vec<LadderEvent> {
    let mut out = Vec::new();
    for r in records {
        let session_id = r.str_field("session_id").unwrap_or_default();
        match r.event.as_str() {
            "rung-entered" => out.push(LadderEvent::RungEntered {
                session_id,
                rung: r.str_field("rung").unwrap_or_default(),
            }),
            "rung-succeeded" => out.push(LadderEvent::RungSucceeded {
                session_id,
                rung: r.str_field("rung").unwrap_or_default(),
            }),
            "rung-timeout" => out.push(LadderEvent::RungTimeout {
                session_id,
                rung: r.str_field("rung").unwrap_or_default(),
                waited_ms: r.u64_field("waited_ms").unwrap_or(0),
            }),
            "recovery-crit" => out.push(LadderEvent::Crit {
                session_id,
                consecutive_failures: r.u64_field("consecutive_failures").unwrap_or(0) as u32,
            }),
            _ => {}
        }
    }
    out
}

// ===========================================================================
// §4.4 — reader (lock-free, torn-tolerant)
// ===========================================================================

/// One parsed event record from the file (§4.4). Carries the raw envelope fields
/// the reader keys on plus the full parsed object for payload inspection.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub pid: u32,
    pub seq: u64,
    pub event: String,
    pub ts: Option<String>,
    pub session: Option<String>,
    pub name: Option<String>,
    /// The full parsed object (for payload field access by consumers).
    pub obj: Map<String, Value>,
}

impl EventRecord {
    /// A payload field as a string (None if absent/wrong-typed).
    pub fn str_field(&self, key: &str) -> Option<String> {
        match self.obj.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// A payload field as u64 (None if absent/wrong-typed).
    pub fn u64_field(&self, key: &str) -> Option<u64> {
        self.obj.get(key).and_then(Value::as_u64)
    }

    /// A payload/envelope field as i64 (None if absent/wrong-typed). The RF-6
    /// start_ms arm reads the envelope's `start_ms` through this.
    pub fn i64_field(&self, key: &str) -> Option<i64> {
        self.obj.get(key).and_then(Value::as_i64)
    }

    /// This record's `send_id`, if it carries one.
    pub fn send_id(&self) -> Option<String> {
        self.str_field("send_id")
    }
}

/// The result of reading one events file (§4.4): the parseable records in file
/// order plus a forensic count of unparseable INTERIOR lines (corruption, not
/// in-flight — the torn TRAILING line is skipped silently and NOT counted).
#[derive(Debug, Clone, Default)]
pub struct ReadResult {
    pub records: Vec<EventRecord>,
    /// Unparseable interior lines — FORENSIC ONLY, never a verdict input (§4.4).
    pub corrupt_interior: u64,
    /// True if the file contained an `events-truncated` marker (§4.3 — reader
    /// WARNs once).
    pub saw_truncation: bool,
}

/// Parse the events-file `text` (§4.4). The LAST line, if unparseable OR missing
/// its `\n`, is a TORN TAIL → skipped SILENTLY (a concurrent ≤4KB append in
/// flight — normal). An unparseable INTERIOR line is skipped with a forensic
/// count (corruption). PURE over the text; nothing unwraps on external data.
pub fn parse_events(text: &str) -> ReadResult {
    let mut result = ReadResult::default();
    if text.is_empty() {
        return result;
    }
    // Whether the final segment is a torn (unterminated) write.
    let trailing_torn = !text.ends_with('\n');
    let mut segments: Vec<&str> = text.split('\n').collect();
    // split on a trailing '\n' yields a final empty segment — drop it.
    if !trailing_torn {
        segments.pop();
    }
    let last_idx = segments.len().saturating_sub(1);

    for (i, seg) in segments.iter().enumerate() {
        let trimmed = seg.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_one(trimmed) {
            Some(rec) => {
                if rec.event == "events-truncated" {
                    result.saw_truncation = true;
                }
                result.records.push(rec);
            }
            None => {
                // The LAST segment, when the file's tail was torn, is an in-flight
                // append → skip SILENTLY (§4.4). Any earlier unparseable line is
                // forensic corruption.
                if i == last_idx && trailing_torn {
                    // silent torn tail
                } else {
                    result.corrupt_interior += 1;
                }
            }
        }
    }
    result
}

/// Parse one trimmed JSON line into an [`EventRecord`], or `None` if it is not a
/// well-formed top-level event object (no top-level `event` string / non-object).
fn parse_one(line: &str) -> Option<EventRecord> {
    let v: Value = serde_json::from_str(line).ok()?;
    let Value::Object(obj) = v else {
        return None;
    };
    let event = match obj.get("event") {
        Some(Value::String(s)) => s.clone(),
        _ => return None,
    };
    // pid/seq are required envelope fields; a record missing them is malformed.
    let pid = obj.get("pid").and_then(Value::as_u64)? as u32;
    let seq = obj.get("seq").and_then(Value::as_u64)?;
    let ts = match obj.get("ts") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let session = match obj.get("session") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let name = match obj.get("name") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    Some(EventRecord {
        pid,
        seq,
        event,
        ts,
        session,
        name,
        obj,
    })
}

/// Read + merge the events file(s) for `(session_id?, name?)` (§4.1 / §4.4): both
/// files are read (either may be absent → empty), records concatenated in
/// (sessionId-file, byname-file) order. Missing/unreadable files contribute
/// nothing (never an error — best-effort read).
pub fn read_merged(state_dir: &Path, session_id: Option<&str>, name: Option<&str>) -> ReadResult {
    let mut merged = ReadResult::default();
    for path in reader_paths(state_dir, session_id, name) {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let r = parse_events(&text);
        merged.records.extend(r.records);
        merged.corrupt_interior += r.corrupt_interior;
        merged.saw_truncation |= r.saw_truncation;
    }
    merged
}

/// Per-pid (pid, seq) ordering helper (§4.4): records for ONE pid sorted by seq.
/// Cross-pid order is file order (forensic only); this is the per-writer view.
pub fn per_pid_ordered(records: &[EventRecord], pid: u32) -> Vec<&EventRecord> {
    let mut v: Vec<&EventRecord> = records.iter().filter(|r| r.pid == pid).collect();
    v.sort_by_key(|r| r.seq);
    v
}

/// The FIRST terminal record for `send_id` in file-read order, if any (§3 /
/// §6 idempotence: readers take the first terminal as the verdict; later
/// terminals are forensic). `None` → the send is dangling.
pub fn first_terminal_for(records: &[EventRecord], send_id: &str) -> Option<EventRecord> {
    records
        .iter()
        .find(|r| is_terminal(&r.event) && r.send_id().as_deref() == Some(send_id))
        .cloned()
}

/// The `send-initiated` record for `send_id`, if present (the recovery-read
/// anchor / dead-writer subject).
pub fn send_initiated_for(records: &[EventRecord], send_id: &str) -> Option<EventRecord> {
    records
        .iter()
        .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(send_id))
        .cloned()
}

// ===========================================================================
// §7 — reader-side dead-writer verdict rule
// ===========================================================================

/// Is the `send-initiated` record `si` DEAD-DANGLING (§7), given the full record
/// set and `now_ms`? Iff: (a) no terminal for its send_id; (b) the original WRITER
/// INCARNATION is gone ([`writer_incarnation_gone`], the RF-6 start_ms-guarded
/// check — NOT bare pid-liveness); (c) age (now − record ts) > [`T_ANCHOR_IDLE_MS`].
///
/// ## RF-6 (R3d) — the start_ms arm closing the v1 named imperfection
/// v1 keyed (b) on bare pid-liveness (`is_pid_alive`), so a recycled pid (the old
/// pid now held by a DIFFERENT process) read as "alive" and SUPPRESSED the trigger
/// — a genuinely dead-dangling send stayed dangling forever once its pid was
/// reused. The start_ms arm folds the writer's recorded process start-time
/// ([`Envelope::start_ms`]) against the live pid's CURRENT start-time: a drift
/// beyond [`crate::kill::START_TIME_SLACK_MS`] proves the live process is a
/// stranger (the original writer is gone) and the trigger is no longer suppressed.
/// This mirrors `kill::pid_is_foreign`'s sound start-time arm. Records without a
/// recorded `start_ms` (older/test records, or an unreadable live start) fall back
/// to bare pid-liveness — the v1 FAIL-SAFE direction (delays, never spuriously
/// fires).
pub fn is_dead_dangling(records: &[EventRecord], si: &EventRecord, now_ms: i64) -> bool {
    let Some(send_id) = si.send_id() else {
        return false;
    };
    // (a) no terminal for this send_id.
    if first_terminal_for(records, &send_id).is_some() {
        return false;
    }
    // (b) the WRITER INCARNATION (pid + start_ms, RF-6) is gone.
    if !writer_incarnation_gone(si) {
        return false;
    }
    // (c) age > threshold.
    let Some(ts) = si.ts.as_deref().and_then(iso_to_epoch_ms) else {
        return false;
    };
    now_ms - ts > T_ANCHOR_IDLE_MS
}

/// Is the SPECIFIC incarnation that wrote `si` gone (RF-6, R3d)? The dead-writer
/// rule's (b) clause, start_ms-guarded:
/// - pid not alive → the writer is gone (the v1 ESRCH case);
/// - pid alive AND a recorded `start_ms` whose live current start has drifted
///   beyond [`crate::kill::START_TIME_SLACK_MS`] → a recycled pid: a STRANGER holds
///   it, the original writer is gone (the arm that closes the imperfection);
/// - pid alive with matching/within-slack start, or no usable start evidence →
///   the writer is (treated as) still alive (v1 fail-safe: no spurious trigger).
fn writer_incarnation_gone(si: &EventRecord) -> bool {
    let pid = si.pid as i32;
    if !is_pid_alive(pid) {
        return true; // pid gone → writer incarnation gone
    }
    // Our OWN live pid is, by construction, the SAME incarnation (this process is
    // running, so its pid cannot have been recycled out from under us). Short-circuit
    // here so the common `await_received` self-wait (the live writer polling for its
    // own terminal) never pays a per-poll `proc_start_ms` (`ps`) spawn — the start_ms
    // arm only matters for a FOREIGN pid that may be a recycled stranger.
    if pid == std::process::id() as i32 {
        return false;
    }
    // A FOREIGN alive pid MAY be a recycled stranger (the recovery-reader case) — the
    // RF-6 start_ms arm. Compare the writer's recorded process start-time against the
    // live pid's CURRENT start-time.
    let Some(recorded) = si.i64_field("start_ms") else {
        return false; // no recorded start (v1/older record) → fall back to pid-alive
    };
    match crate::effects::proc_start_ms(pid) {
        // The live process started no later than the writer (+slack) → SAME
        // incarnation → writer alive. A LATER start (beyond slack) → recycled pid,
        // the original writer is gone (mirrors kill::pid_is_foreign's start arm).
        Some(current) => current > recorded + crate::kill::START_TIME_SLACK_MS,
        // Can't read the live start → fail-safe: treat as alive (no spurious fire).
        None => false,
    }
}

// ===========================================================================
// §6 — recovery-read (the CORRECTED chunk-wise algorithm)
// ===========================================================================

/// Injected deps for [`recovery_read`] (§6): pure over fs reads + clock so the
/// algorithm is unit-testable with planted transcript text. Mirrors
/// [`crate::sendpty::WaitDeps`]'s seam style.
pub trait RecoveryDeps {
    /// Read the transcript text at `path` (the offset-present window source), or
    /// `None` if unreadable.
    fn read_transcript(&self, path: &str) -> Option<String>;
    /// Re-resolve the transcript path NOW for the offset-absent path (§6.1:
    /// registry → find_jsonl_path); `None` if unresolvable.
    fn resolve_transcript(&self, session_id: Option<&str>, name: Option<&str>) -> Option<String>;
    /// Now, ms (the offset-absent pre-send-timestamp skew window).
    fn now_ms(&self) -> i64;
}

/// The §6 recovery verdict — the FOUR epistemically-distinct terminus states
/// (R6 seam ruling 01KX8MDPDX). The verb's terminus closes on **exhausted
/// best-effort + disclosure**, never on "provably didn't land" (structurally
/// unattainable from the recovery keys). Positive matches self-evidence; absence
/// is evidence ONLY relative to a searched, NON-EMPTY window — so the single
/// pre-R6 foreclosing `Abandoned` splits by epistemic state:
/// - (a) [`SourceUnavailable`] — could not read/resolve → NO terminal;
/// - (b) [`EmptyWindow`] — read OK, zero candidates past the anchor → NO terminal
///   (still growable — busy-turn flush lag / rotation-in-place);
/// - (c) [`Abandoned`] — candidates existed, none matched → the disclosed
///   best-effort closer (`pending-abandoned{recovery-no-candidate}` +
///   `recovered:true` + attribution);
/// - (d) [`Unattributable`] — no `content_sha256`, a search can never run →
///   `pending-abandoned{recovery-unattributable}`.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryVerdict {
    /// An exact-sha candidate landed (§6.2). Carries the anchor + attribution.
    Anchored { anchor: Anchor, attribution: String },
    /// A chunk-prefix-truncated candidate landed (§6.3).
    Truncated {
        expected_len: u64,
        actual_len: u64,
        actual_sha: String,
        attribution: String,
    },
    /// (c) SEARCHED, no match (§6.4): candidates existed past the anchor; none
    /// matched exact-sha (§6.2) or chunk-prefix (§6.3). Exhausted best-effort — the
    /// strongest non-delivery evidence the recovery keys can yield. Emits the
    /// DISCLOSED `pending-abandoned{recovery-no-candidate}` stamped `recovered:true`
    /// + the search `attribution` (offset | time-window). The ONLY legitimate
    /// foreclosing recovery terminal.
    Abandoned { attribution: String },
    /// (a) read/resolve FAILURE (§6.1 `build_window` → None): the transcript could
    /// not be read or resolved. Undetermined → emits NO terminal; the send stays
    /// dead-dangling-recoverable for a later run.
    SourceUnavailable,
    /// (b) EMPTY window (§6.1): the read succeeded but ZERO candidate user-records
    /// exist past the send's anchor / in the time-window. The recipient has not
    /// demonstrably progressed past the send — nothing was searched and the window
    /// is still GROWABLE (busy-turn flush lag below the 30s eligibility gate;
    /// rotation-in-place leaving a readable shorter file with the offset past EOF).
    /// Undetermined, exactly like SourceUnavailable in kind → emits NO terminal; a
    /// later sweep resolves it once the window grows.
    EmptyWindow,
    /// (d) MISSING `content_sha256` (§6): a legacy/foreign `send-initiated` with no
    /// recovery key — a search can never run (trivially exhausted). Closing is
    /// legitimate but must NOT claim "no-candidate"; emits
    /// `pending-abandoned{recovery-unattributable}` (no `recovered`/attribution).
    Unattributable,
}

/// The window source for recovery-read (§6.1): offset-present (strong) vs
/// offset-absent (time-window, weak).
enum Window {
    /// Offset present → attribution "offset"; candidates are records past offset.
    Offset {
        transcript: String,
        text: String,
        offset: u64,
    },
    /// Offset absent → attribution "time-window"; candidates filtered by ts skew.
    TimeWindow { transcript: String, text: String },
}

/// A windowed candidate user-record: its text + the byte offset where its line
/// begins (the anchor start_offset) + its index in the windowed slice. The
/// time-window exclusion (§6.1/R6) is applied DURING extraction, so the kept
/// candidates carry no timestamp here.
struct Candidate {
    text: String,
    start_offset: u64,
    line_index: u64,
}

/// Recovery-read (§6) over a DANGLING `send-initiated` record `si`. Returns the
/// verdict; the CALLER appends the late event (so idempotence's re-check-then-
/// append stays at the call site — see [`emit_recovery_verdict`]). Pure over
/// `deps`.
pub fn recovery_read(deps: &dyn RecoveryDeps, si: &EventRecord) -> RecoveryVerdict {
    // Pull the recovery inputs off the send-initiated record.
    // (d) MISSING content_sha256 — a legacy/foreign record with no recovery key; a
    // search can never run. Close honestly as UNATTRIBUTABLE (never "no-candidate").
    let content_sha = match si.str_field("content_sha256") {
        Some(s) => s,
        None => return RecoveryVerdict::Unattributable,
    };
    let content_len = si.u64_field("content_len").unwrap_or(0);
    let chunk_shas: Vec<String> = match si.obj.get("chunk_sha256s") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    // §6.1 — build the candidate window.
    // (a) read/resolve FAILURE — could not read the offset-present transcript, or
    // could not resolve/read the offset-absent one. UNDETERMINED → NO terminal.
    let window = match build_window(deps, si) {
        Some(w) => w,
        None => return RecoveryVerdict::SourceUnavailable,
    };
    let (transcript, attribution, candidates) = window_candidates(deps, window, si);
    // (b) EMPTY window — the read SUCCEEDED but zero candidate user-records sit past
    // the send's anchor / in the time-window. The recipient has not demonstrably
    // progressed past the send; nothing was searched; the window is still growable
    // (flush lag / rotation-in-place). UNDETERMINED, like (a) → NO terminal. This is
    // DISTINCT from (c) below — the crux of R6: absence is evidence only relative to
    // a searched, NON-EMPTY window.
    if candidates.is_empty() {
        return RecoveryVerdict::EmptyWindow;
    }

    // §6.2 — exact match (first candidate whose sha == content_sha256).
    for c in &candidates {
        if sha256_hex(c.text.as_bytes()) == content_sha {
            return RecoveryVerdict::Anchored {
                anchor: Anchor {
                    transcript: transcript.clone(),
                    start_offset: c.start_offset,
                    line_index: c.line_index,
                },
                attribution,
            };
        }
    }

    // §6.3 — chunk-prefix truncation: longest matched prefix wins.
    let mut best: Option<(&Candidate, usize)> = None; // (candidate, matched full chunks)
    for c in &candidates {
        if (c.text.len() as u64) >= content_len {
            continue; // not shorter → not a truncation candidate
        }
        let matched = matched_full_chunk_prefix(&c.text, &chunk_shas);
        if matched >= 1 {
            match best {
                Some((_, bm)) if bm >= matched => {}
                _ => best = Some((c, matched)),
            }
        }
    }
    if let Some((c, _)) = best {
        return RecoveryVerdict::Truncated {
            expected_len: content_len,
            actual_len: c.text.len() as u64,
            actual_sha: sha256_hex(c.text.as_bytes()),
            attribution,
        };
    }

    // (c) §6.4 — candidates existed but NONE matched exact-sha or chunk-prefix. The
    // recipient demonstrably consumed turns past the send's offset and the content is
    // not among them: exhausted best-effort. The disclosed closer, carrying the
    // search `attribution` so the terminal reads through D4's "recovered (attributed)"
    // category, never a hard "failed".
    RecoveryVerdict::Abandoned { attribution }
}

/// Count how many FULL leading chunks of `text` (re-chunked with CHUNK_BYTES)
/// match `chunk_shas` in order (§6.3). A trailing chunk of EXACTLY CHUNK_BYTES
/// counts as full; a shorter trailing chunk is a partial (excluded). Returns the
/// number of matched full chunks (the matched-prefix length).
///
/// NAMED LIMITATION L5/R8: a truncation landing mid-multibyte-codepoint can shift
/// the candidate's chunk boundaries vs the original's by up to 3 bytes for
/// non-ASCII payloads, degrading detection to the L1 miss class — named, not
/// silent.
fn matched_full_chunk_prefix(text: &str, chunk_shas: &[String]) -> usize {
    let chunks = chunk_text(text, CHUNK_BYTES);
    let mut matched = 0usize;
    for (i, chunk) in chunks.iter().enumerate() {
        // A trailing chunk shorter than CHUNK_BYTES is a partial → stop (only
        // FULL chunks are comparable, §6.3).
        if chunk.len() < CHUNK_BYTES {
            break;
        }
        match chunk_shas.get(i) {
            Some(expected) if sha256_hex(chunk.as_bytes()) == *expected => matched += 1,
            // A mismatch or a missing expected sha (capped array) stops the run.
            _ => break,
        }
    }
    matched
}

/// Resolve the candidate window for `si` (§6.1): offset-present → the transcript
/// path + offset off the record; offset-absent → re-resolve + a time-window.
fn build_window(deps: &dyn RecoveryDeps, si: &EventRecord) -> Option<Window> {
    let transcript = si.str_field("transcript");
    let offset = si.u64_field("transcript_offset");
    match (transcript, offset) {
        (Some(t), Some(o)) => {
            let text = deps.read_transcript(&t)?;
            Some(Window::Offset {
                transcript: t,
                text,
                offset: o,
            })
        }
        // Offset absent (or transcript absent) → re-resolve NOW + time-window.
        _ => {
            let t = deps.resolve_transcript(si.session.as_deref(), si.name.as_deref())?;
            let text = deps.read_transcript(&t)?;
            Some(Window::TimeWindow {
                transcript: t,
                text,
            })
        }
    }
}

/// Turn a [`Window`] into `(transcript_path, attribution, candidates)` (§6.1).
/// Offset path: user-records whose line begins at/after `offset` (the find_user_
/// anchor "first past the start offset" guarantee). Time-window path: records
/// whose raw `timestamp` ≥ send ts − 5s skew; records BEFORE the send are
/// EXCLUDED (R6 — an identical earlier message must not be claimed).
fn window_candidates(
    deps: &dyn RecoveryDeps,
    window: Window,
    si: &EventRecord,
) -> (String, String, Vec<Candidate>) {
    match window {
        Window::Offset {
            transcript,
            text,
            offset,
        } => {
            let candidates = extract_candidates(&text, Some(offset), None);
            (transcript, "offset".to_string(), candidates)
        }
        Window::TimeWindow { transcript, text } => {
            // The send ts (envelope) minus a 5s skew is the exclusion floor.
            let send_ts = si.ts.as_deref().and_then(iso_to_epoch_ms);
            let floor = send_ts.map(|t| t - 5_000).unwrap_or(i64::MIN);
            let _ = deps; // deps.now_ms() unused here; floor is send-ts relative.
            let candidates = extract_candidates(&text, None, Some(floor));
            (transcript, "time-window".to_string(), candidates)
        }
    }
}

/// Extract user-record candidates from transcript `text` (§6.1). When
/// `min_offset` is Some, only records whose LINE START byte is ≥ the offset are
/// kept (offset path). When `ts_floor` is Some, only records whose raw
/// `timestamp` ms ≥ floor are kept (time-window path; records without a timestamp
/// are weak-attribution candidates that are still INCLUDED per §6.1, but a
/// present-and-earlier timestamp is EXCLUDED).
///
/// Reuses the SAME extractors as the live anchor: [`parse_jsonl_slice`] +
/// [`user_record_text`] (one mechanical definition, §6.1).
fn extract_candidates(
    text: &str,
    min_offset: Option<u64>,
    ts_floor: Option<i64>,
) -> Vec<Candidate> {
    // The slice we parse: offset path takes the bytes AT/after the offset (the
    // find_user_anchor "past the offset" window); the time-window path parses the
    // whole file and filters by timestamp.
    let (slice, base_offset) = match min_offset {
        Some(o) => {
            let o = (o as usize).min(text.len());
            // Don't split a multibyte codepoint: walk forward to a char boundary.
            let mut start = o;
            while start < text.len() && !text.is_char_boundary(start) {
                start += 1;
            }
            (&text[start..], start as u64)
        }
        None => (text, 0u64),
    };
    // THE SAME pipeline the live anchor reads (§6.1): parse_jsonl_slice →
    // record-view → user_record_text. parse_jsonl_slice drops blank lines, so we
    // re-walk the slice's own raw lines IN PARALLEL to recover each kept line's
    // absolute byte offset (the anchor start_offset contract).
    let parsed = parse_jsonl_slice(slice);
    let mut out = Vec::new();
    let mut parsed_it = parsed.iter();
    let mut next_parsed = parsed_it.next();
    let mut cursor = base_offset;
    let mut line_index = 0u64;
    for raw_line in slice.split_inclusive('\n') {
        let line_start = cursor;
        cursor += raw_line.len() as u64;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue; // parse_jsonl_slice also drops blanks — stay aligned.
        }
        // Consume the matching ParsedLine (raw text is the trimmed line, modulo
        // the original whitespace parse_jsonl_slice trims for emptiness only).
        let Some(p) = next_parsed else { break };
        next_parsed = parsed_it.next();

        let rec: JsonlRecord = serde_json::from_value(p.value.clone()).unwrap_or_default();
        let Some(user_text) = user_record_text(&rec) else {
            line_index += 1;
            continue;
        };
        // Time-window exclusion (§6.1/R6): a present-and-earlier timestamp is out.
        if let Some(floor) = ts_floor {
            let ts_ms = p
                .value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(iso_to_epoch_ms);
            if let Some(t) = ts_ms {
                if t < floor {
                    line_index += 1;
                    continue; // earlier than the send → excluded (no false-Anchor)
                }
            }
            // timestamp absent → weak candidate, INCLUDED (§6.1).
        }
        out.push(Candidate {
            text: user_text,
            start_offset: line_start,
            line_index,
        });
        line_index += 1;
    }
    out
}

/// Build the late terminal event a recovery verdict produces, or `None` when the
/// verdict mints NO terminal (R6 seam ruling 01KX8MDPDX). Used by
/// [`emit_recovery_verdict`] and by `await_received`'s inline recovery.
///
/// - Anchored/Truncated (§6.2-6.3) → the recovered anchor terminals (unchanged).
/// - (c) `Abandoned` (§6.4 searched-no-match) → `pending-abandoned{recovery-no-
///   candidate}` STAMPED `recovered:true` + the search `attribution` — the disclosed
///   best-effort closer (D4's "recovered (attributed)" category).
/// - (d) `Unattributable` → `pending-abandoned{recovery-unattributable}`, NO
///   `recovered`/attribution (no search ran; never claims "no-candidate").
/// - (a) `SourceUnavailable` / (b) `EmptyWindow` → `None`: NO terminal, the send
///   stays dead-dangling-recoverable for a later run (like the G/B door arms).
pub fn recovery_event(
    send_id: &str,
    content_sha256: &str,
    verdict: &RecoveryVerdict,
) -> Option<Payload> {
    match verdict {
        RecoveryVerdict::Anchored {
            anchor,
            attribution,
        } => Some(Payload::TurnAnchored {
            send_id: send_id.to_string(),
            content_sha256: content_sha256.to_string(),
            anchor: anchor.clone(),
            recovered: true,
            attribution: Some(attribution.clone()),
        }),
        RecoveryVerdict::Truncated {
            expected_len,
            actual_len,
            actual_sha,
            attribution,
        } => Some(Payload::TurnAnchoredMismatch {
            send_id: send_id.to_string(),
            expected_sha: content_sha256.to_string(),
            actual_sha: actual_sha.clone(),
            expected_len: *expected_len,
            actual_len: *actual_len,
            recovered: true,
            attribution: Some(attribution.clone()),
        }),
        // (c) DISCLOSED best-effort closer — the ONLY legitimate foreclosing recovery
        // terminal. recovered:true + attribution route a reader to D4's attributed
        // category (QS-1 converse: no UNDISCLOSED false-abandoned).
        RecoveryVerdict::Abandoned { attribution } => Some(Payload::PendingAbandoned {
            send_id: send_id.to_string(),
            reason: "recovery-no-candidate".to_string(),
            recovered: Some(true),
            attribution: Some(attribution.clone()),
        }),
        // (d) legacy/foreign no-key record — closes honestly WITHOUT claiming a search
        // ran (no recovered/attribution; distinct reason).
        RecoveryVerdict::Unattributable => Some(Payload::PendingAbandoned {
            send_id: send_id.to_string(),
            reason: "recovery-unattributable".to_string(),
            recovered: None,
            attribution: None,
        }),
        // (a),(b) UNDETERMINED → NO terminal (dead-dangling-recoverable).
        RecoveryVerdict::SourceUnavailable | RecoveryVerdict::EmptyWindow => None,
    }
}

/// Run recovery-read for the dangling `si` and append its late event IDEMPOTENTLY
/// (§6 idempotence): re-read the file(s), re-check the dangling predicate
/// immediately BEFORE appending — if a terminal has appeared (another reader raced
/// us), do nothing. Best-effort non-fatal. Returns the verdict that holds (the
/// existing terminal's, if one raced in).
pub fn emit_recovery_verdict(
    deps: &dyn RecoveryDeps,
    writer: &EventWriter,
    clock: &dyn Clock,
    ctx: ReaderCtx,
    si: &EventRecord,
) -> Result<RecoveryVerdict, String> {
    let send_id = si.send_id().ok_or("send-initiated has no send_id")?;
    let content_sha = si.str_field("content_sha256").unwrap_or_default();
    // recovery_read is a pure TRANSCRIPT read (never touches the events file), so it
    // stays OUTSIDE the lock — its verdict is only USED if no terminal raced in.
    let verdict = recovery_read(deps, si);
    // §C2 / F2 — serialize the re-check→emit critical section ACROSS PROCESSES. The
    // v1 idempotence was a lock-free read-then-append: two concurrent `qd
    // delivery:recover` runs (or one racing the deferred `recovery_coordinator`) both
    // passed the re-check and both appended → TWO terminals for one send_id (C2
    // "exactly one" broken, red-team F2). An exclusive advisory flock on the emit
    // target, held across the re-check AND the emit, forces the second caller to
    // BLOCK until the first releases; it then re-reads, observes the first's terminal,
    // and takes the idempotent adopt-path. This also closes the round-1 outcome-FLIP
    // (differing verdicts): the second caller adopts the first's terminal regardless
    // of its own recovery_read result. Fail-safe: a flock failure returns Err and
    // emits nothing (the send stays dangling-recoverable, best-effort §4.2). The
    // FENCE never reaches here for a live writer, so the live-writer-refusal path
    // never touches the lock.
    let _lock = RecoveryEmitLock::acquire(writer.path())?;
    // Idempotence re-check UNDER the lock: re-read NOW; if a terminal raced in, take it.
    let merged = ctx.read();
    if let Some(existing) = first_terminal_for(&merged.records, &send_id) {
        return Ok(verdict_from_terminal(&existing));
    }
    // SourceUnavailable / EmptyWindow → recovery_event returns None: emit NO terminal,
    // leaving the send dead-dangling-recoverable (R6 (a)/(b)). We still ran the lock +
    // idempotence re-check above, so if another run resolved it (its read succeeded) we
    // already adopted that terminal on the raced-in path. Every other verdict emits its
    // (disclosed) terminal.
    if let Some(payload) = recovery_event(&send_id, &content_sha, &verdict) {
        writer.emit(clock, &payload)?;
    }
    Ok(verdict)
}

/// Cross-process advisory lock over the recovery re-check→emit critical section
/// (§C2 / F2). `flock(LOCK_EX)` on the emit-target events file, held for the guard's
/// life and released when the owned `File` drops (every early return of
/// [`emit_recovery_verdict`]). Mirrors [`crate::idstore`]'s `open_locked` — the same
/// read-check-then-append-under-one-lock idiom. Advisory (flock) locks only contend
/// with OTHER flock callers, so the normal O_APPEND emission sites (send-initiated,
/// chunks-delivered, …) are unaffected — only recovery emitters serialize, which is
/// exactly the scope C2 needs (the fence already excludes a live sender racing).
struct RecoveryEmitLock {
    _file: std::fs::File,
}

impl RecoveryEmitLock {
    fn acquire(events_path: &Path) -> Result<Self, String> {
        use std::os::unix::io::AsRawFd;
        if let Some(parent) = events_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(events_path)
            .map_err(|e| format!("recovery lock: open {} failed: {e}", events_path.display()))?;
        // Blocking exclusive lock (LOCK_EX) — the second caller waits here, then its
        // re-check sees the first's terminal. Released on `File` drop.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(format!(
                "recovery lock: flock {} failed: {}",
                events_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(RecoveryEmitLock { _file: file })
    }
}

/// Map an existing terminal record back to a [`RecoveryVerdict`] (the idempotence
/// "another reader already wrote the verdict" path). A timeout/abandoned terminal
/// is reported as Abandoned for the recovery caller (no anchor produced).
fn verdict_from_terminal(rec: &EventRecord) -> RecoveryVerdict {
    match rec.event.as_str() {
        "turn-anchored" => {
            let anchor = rec
                .obj
                .get("anchor")
                .and_then(parse_anchor)
                .unwrap_or(Anchor {
                    transcript: String::new(),
                    start_offset: 0,
                    line_index: 0,
                });
            RecoveryVerdict::Anchored {
                anchor,
                attribution: rec.str_field("attribution").unwrap_or_default(),
            }
        }
        "turn-anchored-mismatch" => RecoveryVerdict::Truncated {
            expected_len: rec.u64_field("expected_len").unwrap_or(0),
            actual_len: rec.u64_field("actual_len").unwrap_or(0),
            actual_sha: rec.str_field("actual_sha").unwrap_or_default(),
            attribution: rec.str_field("attribution").unwrap_or_default(),
        },
        // §C1 — a door `send-failed` is a failure with no anchor: Abandoned for the
        // recovery caller (explicit, not via the catch-all — the terminal set grew).
        "send-failed" => RecoveryVerdict::Abandoned {
            attribution: String::new(),
        },
        // anchor-timeout / pending-abandoned / message-seen / seen-failed → no anchor.
        // A raced-in disclosed `pending-abandoned{recovery-no-candidate}` carries an
        // `attribution`; adopt it (others default empty). This is the idempotence
        // adopt-path — the returned verdict only informs the caller a terminal exists.
        _ => RecoveryVerdict::Abandoned {
            attribution: rec.str_field("attribution").unwrap_or_default(),
        },
    }
}

fn parse_anchor(v: &Value) -> Option<Anchor> {
    let o = v.as_object()?;
    Some(Anchor {
        transcript: o.get("transcript")?.as_str()?.to_string(),
        start_offset: o.get("start_offset")?.as_u64()?,
        line_index: o.get("line_index")?.as_u64()?,
    })
}

// ===========================================================================
// §8 — await_received
// ===========================================================================

/// The terminal outcome of [`await_received`] (§8).
#[derive(Debug, Clone, PartialEq)]
pub enum Received {
    Anchored,
    AnchoredMismatch,
    AnchorTimeout,
    Abandoned,
    /// §C1 — a `send-failed` door terminal resolved the await (an explicit pre-wire
    /// failure). Distinct from `Abandoned` (a watch that ended with no verdict): the
    /// door failed loudly. `reason` carries the door's token (forensics). A
    /// consumer must treat it as a FAILURE, never a success — and never hang: a
    /// `send-failed` is a terminal, so it satisfies the await.
    SendFailed {
        reason: String,
    },
    /// The await budget was exhausted with no terminal AND no dead-dangling
    /// resolution. `last_stage` carries the furthest non-terminal stage seen
    /// (forensics) — NEVER a success (§8 / G4 cheap-event trap).
    BudgetExhausted {
        last_stage: &'static str,
    },
}

/// The await budget (§8). `max_polls` bounds the poll loop (each poll = one
/// `poll_ms` sleep). `idle` selects the 30s idle default vs the progress-keyed
/// busy path; in this library form the caller supplies the outer cap as
/// `max_polls` (busy callers pass a large cap; the test harness passes small).
#[derive(Debug, Clone)]
pub struct AwaitBudget {
    pub poll_ms: u64,
    pub max_polls: u64,
}

impl Default for AwaitBudget {
    fn default() -> Self {
        // 30s idle default at 500ms cadence (§8).
        AwaitBudget {
            poll_ms: 500,
            max_polls: 60,
        }
    }
}

/// Injected deps for [`await_received`] (§8): the poll loop's sleep/clock seam
/// (mirrors [`crate::sendpty::WaitDeps`]) PLUS the recovery deps (a dead-dangling
/// poll runs recovery inline).
pub trait AwaitDeps: RecoveryDeps {
    /// Sleep `ms` (the poll cadence; seamed so tests run instantly).
    fn sleep(&self, ms: u64);
}

/// Await the FIRST terminal event for `send_id` (§8). Bounded poll of the merged
/// event file(s); returns on the first `is_terminal` event for `send_id` and on
/// NOTHING else (the match arm exists only for terminal kinds — the cheap-event
/// trap stays structurally closed). Each poll applies §7: a dead-dangling send
/// runs recovery-read inline and returns its verdict. On budget exhaustion EMITS
/// `anchor-timeout {waited_ms}` (timeouts stay positive events, §8) and returns
/// `AnchorTimeout`.
pub fn await_received(
    deps: &dyn AwaitDeps,
    clock: &dyn Clock,
    writer: &EventWriter,
    ctx: ReaderCtx,
    send_id: &str,
    budget: AwaitBudget,
) -> Received {
    let start_ms = clock.now_ms();
    let mut last_stage = "send-initiated";

    for _ in 0..budget.max_polls {
        let merged = ctx.read();

        // FIRST terminal for our send_id wins (§3 / §8).
        if let Some(term) = first_terminal_for(&merged.records, send_id) {
            return received_from_terminal(&term);
        }

        // Forensic last_stage (chunks-delivered only; NEVER a success — §8).
        if merged
            .records
            .iter()
            .any(|r| r.event == "chunks-delivered" && r.send_id().as_deref() == Some(send_id))
        {
            last_stage = "chunks-delivered";
        }

        // §7 dead-dangling check: run recovery inline + return its verdict — UNLESS the
        // transcript was unreadable/unresolvable or the window was empty this poll
        // ((a)/(b) → received_from_verdict None): no terminal was emitted, so keep
        // polling (a later poll may resolve it; else the budget exhausts to a positive
        // anchor-timeout below). This is the inline-recovery mirror of the recover verb
        // leaving such a send dead-dangling (R6).
        if let Some(si) = send_initiated_for(&merged.records, send_id) {
            if is_dead_dangling(&merged.records, &si, clock.now_ms()) {
                if let Ok(verdict) = emit_recovery_verdict(deps, writer, clock, ctx, &si) {
                    if let Some(received) = received_from_verdict(&verdict) {
                        return received;
                    }
                }
            }
        }

        deps.sleep(budget.poll_ms);
    }

    // §8 budget exhaustion → EMIT a positive anchor-timeout, return AnchorTimeout.
    let waited_ms = (clock.now_ms() - start_ms).max(0) as u64;
    let _ = writer.emit(
        clock,
        &Payload::AnchorTimeout {
            send_id: send_id.to_string(),
            waited_ms,
        },
    );
    // The cheap-event trap row (G4) keys on this: a stream of ONLY non-terminal
    // events never returns a success variant. We return AnchorTimeout (we emitted
    // it) — a budget-only caller without emission would see BudgetExhausted.
    let _ = last_stage;
    Received::AnchorTimeout
}

/// Map a terminal record to a [`Received`] (§8).
fn received_from_terminal(rec: &EventRecord) -> Received {
    match rec.event.as_str() {
        "turn-anchored" => Received::Anchored,
        "turn-anchored-mismatch" => Received::AnchoredMismatch,
        "anchor-timeout" => Received::AnchorTimeout,
        "pending-abandoned" => Received::Abandoned,
        // §C1 — the door-failure terminal. A KNOWN terminal now, so it maps to an
        // explicit failure (never the unknown-terminal catch-all, never a success).
        "send-failed" => Received::SendFailed {
            reason: rec.str_field("reason").unwrap_or_default(),
        },
        // is_terminal gated the caller, so this is unreachable; be safe.
        _ => Received::BudgetExhausted {
            last_stage: "unknown-terminal",
        },
    }
}

/// Map a recovery verdict to a [`Received`] (§7 inline-recovery return), or `None`
/// when the verdict minted NO terminal — (a) `SourceUnavailable` / (b) `EmptyWindow`:
/// undetermined this poll, no resolution, so [`await_received`] must NOT return; it
/// keeps polling (a later poll may resolve it once the transcript is readable / the
/// window grows; else the budget exhausts to a positive `anchor-timeout`). The
/// terminal-minting verdicts (Anchored / Truncated / (c) Abandoned / (d)
/// Unattributable) resolve the await.
fn received_from_verdict(v: &RecoveryVerdict) -> Option<Received> {
    match v {
        RecoveryVerdict::Anchored { .. } => Some(Received::Anchored),
        RecoveryVerdict::Truncated { .. } => Some(Received::AnchoredMismatch),
        RecoveryVerdict::Abandoned { .. } => Some(Received::Abandoned),
        RecoveryVerdict::Unattributable => Some(Received::Abandoned),
        RecoveryVerdict::SourceUnavailable | RecoveryVerdict::EmptyWindow => None,
    }
}

// ===========================================================================
// §9 — verb-layer emission helpers (M2/M3): warn-emit + the WatchGuard
// ===========================================================================

/// Emit `payload` through `writer`, WARNing to stderr on failure but NEVER
/// changing the caller's exit code (§4.2 best-effort non-fatal; the A6
/// telemetry "WARNING: … (non-fatal)" precedent). The verb wiring (M2/M3) calls
/// this everywhere it emits so the non-fatal contract lives in ONE place.
pub fn warn_emit(writer: &EventWriter, clock: &dyn Clock, payload: &Payload) {
    if let Err(e) = writer.emit(clock, payload) {
        eprintln!("WARNING: event emit failed (non-fatal): {e}");
    }
}

/// Exit finalizer for a STARTED watch (§9 / rev C row 24). Armed when a watch
/// begins (the `--wait` loop, the W8 verify window, the `-p` deliver-acceptance
/// watch); a terminal emission [`WatchGuard::disarm`]s it. If the guard is
/// dropped STILL ARMED — an early `return`, a `?`, or a panic unwind left the
/// watch without a terminal — `Drop` emits `pending-abandoned{watch-interrupted}`
/// so a watched send never silently vanishes.
///
/// SIGKILL / uncaught signals bypass `Drop` and leave a dangling send — by
/// design covered by the reader-side dead-writer rule (§7); that is why row 24
/// has two halves. No signal handlers in v1.
///
/// Generic over the [`Clock`] so the unit rows drive it with a [`FixedClock`];
/// the verb layer passes [`crate::effects::RealClock`].
pub struct WatchGuard<'a, C: Clock> {
    writer: &'a EventWriter,
    clock: &'a C,
    send_id: String,
    armed: bool,
}

impl<'a, C: Clock> WatchGuard<'a, C> {
    /// Arm a guard for `send_id` over `writer`/`clock` (a watch just started).
    pub fn arm(writer: &'a EventWriter, clock: &'a C, send_id: &str) -> Self {
        WatchGuard {
            writer,
            clock,
            send_id: send_id.to_string(),
            armed: true,
        }
    }

    /// Disarm WITHOUT emitting — the watch reached a terminal of its own (the
    /// caller emitted it). Idempotent; consumes the arm.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl<C: Clock> Drop for WatchGuard<'_, C> {
    fn drop(&mut self) {
        if self.armed {
            warn_emit(
                self.writer,
                self.clock,
                &Payload::PendingAbandoned {
                    send_id: self.send_id.clone(),
                    reason: "watch-interrupted".to_string(),
                    // NOT a searched-best-effort recovery verdict — no disclosure flags
                    // (serializes exactly as the pre-R6 bare form; R6 disclosure detail).
                    recovered: None,
                    attribution: None,
                },
            );
        }
    }
}

// ===========================================================================
// ISO-8601 helpers (the reverse of render::epoch_ms_to_iso, for ts comparison)
// ===========================================================================

/// Parse an ISO-8601 UTC ms timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`, the
/// [`epoch_ms_to_iso`] shape OR a transcript's raw `timestamp`) to epoch ms.
/// Best-effort: `None` on any malformed input (used for AGE/exclusion math, never
/// a hard dependency). Handles the optional fractional-second + trailing `Z`.
pub fn iso_to_epoch_ms(s: &str) -> Option<i64> {
    // Expect: date 'T' time ('.' millis)? 'Z'? — be lenient on the suffix.
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let y: i64 = dparts.next()?.parse().ok()?;
    let mo: i64 = dparts.next()?.parse().ok()?;
    let d: i64 = dparts.next()?.parse().ok()?;
    let rest = rest.trim_end_matches('Z');
    // Time may carry a timezone offset in real transcripts; we only support the
    // 'Z'/no-suffix UTC form (the engine's own ts) + bare HH:MM:SS(.mmm).
    let (time, millis) = match rest.split_once('.') {
        Some((t, frac)) => {
            // Take up to 3 leading digits of the fraction as ms.
            let ms: i64 = frac
                .chars()
                .take(3)
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            // pad e.g. "5" → 500, "05" → 50.
            let digits = frac.chars().take(3).count();
            let ms = ms * 10i64.pow((3 - digits) as u32);
            (t, ms)
        }
        None => (rest, 0),
    };
    let mut tparts = time.split(':');
    let h: i64 = tparts.next()?.parse().ok()?;
    let mi: i64 = tparts.next()?.parse().ok()?;
    let se: i64 = tparts.next().unwrap_or("0").parse().ok()?;
    Some(civil_to_epoch_ms(y, mo, d, h, mi, se, millis))
}

/// Days-from-civil (Howard Hinnant), the inverse companion of render.rs's
/// `civil_from_epoch_ms`. Returns epoch ms for a UTC civil datetime.
fn civil_to_epoch_ms(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, millis: i64) -> i64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    ((days * 86400 + h * 3600 + mi * 60 + s) * 1000) + millis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::FixedClock;
    use serde_json::json;
    use std::sync::atomic::AtomicI64;
    use tempfile::tempdir;

    // ---------------------------------------------------------------------
    // §2.1 send_id mint
    // ---------------------------------------------------------------------

    #[test]
    fn mint_send_id_shape_and_monotonic_n() {
        let clock = FixedClock(1_781_241_549_123);
        let a = mint_send_id(&clock);
        let b = mint_send_id(&clock);
        let pid = std::process::id();
        assert!(a.starts_with(&format!("{pid}-1781241549123-")));
        // n strictly increases within the process.
        let na: u64 = a.rsplit('-').next().unwrap().parse().unwrap();
        let nb: u64 = b.rsplit('-').next().unwrap().parse().unwrap();
        assert!(nb > na);
    }

    #[test]
    fn sha256_hex_is_64_lowercase_hex() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Known vector for "hello".
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    // ---------------------------------------------------------------------
    // §3 terminal set
    // ---------------------------------------------------------------------

    #[test]
    fn terminal_set_membership() {
        assert!(is_terminal("turn-anchored"));
        assert!(is_terminal("turn-anchored-mismatch"));
        assert!(is_terminal("anchor-timeout"));
        assert!(is_terminal("pending-abandoned"));
        // The cheap events are NOT terminal (the trap is closed at the set, §3).
        assert!(!is_terminal("chunks-delivered"));
        assert!(!is_terminal("composer-cleared"));
        assert!(!is_terminal("status-transition"));
        assert!(!is_terminal("send-initiated"));
        assert!(!is_terminal("priming-readiness-timeout"));
        // §X (3-phase delivery): the new on-received terminals + the
        // non-terminal on-queued ack.
        assert!(is_terminal("message-seen"));
        assert!(is_terminal("seen-failed"));
        assert!(!is_terminal("relay-delivered"));
    }

    // ---------------------------------------------------------------------
    // §X (3-phase delivery) — relay-delivered / message-seen / seen-failed:
    // U1 (serialize to EXACTLY the §X.3 on-disk shape, pinned key order) +
    // U4 (terminal-class membership). ack3_matrix.rs's coverage_inventory
    // DELEGATES these kinds here by these fn names — keep the names in sync.
    // ---------------------------------------------------------------------

    #[test]
    fn x3_relay_delivered_key_order_and_nonterminal() {
        let p = Payload::RelayDelivered {
            send_id: "relay-1781241549123-7".to_string(),
            content_sha256: sha256_hex(b"the message"),
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        assert_eq!(
            keys_of(&line),
            vec![
                "v",
                "ts",
                "pid",
                "seq",
                "session",
                "name",
                "event",
                "send_id",
                "content_sha256"
            ]
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "relay-delivered");
        // NON-terminal — the relay analog of chunks-delivered (§X.3.2).
        assert!(!is_terminal("relay-delivered"));
    }

    #[test]
    fn x3_message_seen_key_order_and_terminal() {
        let p = Payload::MessageSeen {
            send_id: "relay-1781241549123-7".to_string(),
            content_sha256: sha256_hex(b"the message"),
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        assert_eq!(
            keys_of(&line),
            vec![
                "v",
                "ts",
                "pid",
                "seq",
                "session",
                "name",
                "event",
                "send_id",
                "content_sha256"
            ]
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "message-seen");
        // TERMINAL (success) — §X.3.4.
        assert!(is_terminal("message-seen"));
    }

    #[test]
    fn x3_seen_failed_key_order_and_terminal() {
        let p = Payload::SeenFailed {
            send_id: "relay-1781241549123-7".to_string(),
            reason: "recipient-gone".to_string(),
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        assert_eq!(
            keys_of(&line),
            vec!["v", "ts", "pid", "seq", "session", "name", "event", "send_id", "reason"]
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "seen-failed");
        assert_eq!(v["reason"], "recipient-gone");
        // TERMINAL (failure) — §X.3.5.
        assert!(is_terminal("seen-failed"));
    }

    #[test]
    fn x3_first_terminal_for_picks_message_seen_over_nonterminal() {
        // relay-delivered (non-terminal) then message-seen (terminal), one send_id:
        // first_terminal_for must return the message-seen.
        let sid = "relay-1781241549123-7";
        let l1 = build_record_line(
            &env_for(0),
            &Payload::RelayDelivered {
                send_id: sid.to_string(),
                content_sha256: sha256_hex(b"m"),
            },
            CHUNK_SHA_CAP,
        );
        let l2 = build_record_line(
            &env_for(1),
            &Payload::MessageSeen {
                send_id: sid.to_string(),
                content_sha256: sha256_hex(b"m"),
            },
            CHUNK_SHA_CAP,
        );
        let text = format!("{l1}\n{l2}\n");
        let parsed = parse_events(&text);
        let term = first_terminal_for(&parsed.records, sid).expect("a terminal");
        assert_eq!(term.event, "message-seen");
    }

    #[test]
    fn x3_relay_send_initiated_uses_relay_values() {
        // U2 — §X.3.1: relay REUSES Payload::SendInitiated with relay values
        // (NOT a bare 2-field record). A consumer adopts it as untyped JSON by
        // content_sha256, ignoring verb/send_path; the new diagnostic string
        // values pass through transparently.
        let sha = sha256_hex(b"hello relay");
        let p = Payload::SendInitiated {
            send_id: "relay-1781241549123-7".to_string(),
            verb: "send:relay".to_string(),
            send_path: "relay".to_string(),
            content_sha256: sha.clone(),
            content_len: "hello relay".len() as u64,
            chunks: 1,
            chunk_sha256s: vec![sha.clone()],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "send-initiated");
        assert_eq!(v["verb"], "send:relay");
        assert_eq!(v["send_path"], "relay");
        assert_eq!(v["chunks"], 1);
        assert_eq!(v["chunk_sha256s"], serde_json::json!([sha]));
        assert_eq!(v["content_sha256"], serde_json::Value::String(sha));
        // Relay carries NO prose (§X.7) and the sender has no recovery transcript
        // (§X.3.1) → these are OMITTED; capped=false omitted too.
        assert!(!line.contains("content_preview"));
        assert!(!line.contains("transcript"));
        assert!(!line.contains("chunk_sha256s_capped"));
    }

    // ---------------------------------------------------------------------
    // G1: per-payload serde round-trip + pinned key order + ≤4KB bound
    // ---------------------------------------------------------------------

    fn env_for(seq: u64) -> Envelope {
        Envelope {
            v: 1,
            ts: "2026-06-06T06:09:00.123Z".to_string(),
            pid: 71234,
            seq,
            session: Some("11111111-2222-3333-4444-555555555555".to_string()),
            name: Some("alpha".to_string()),
            start_ms: None,
        }
    }

    fn keys_of(line: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(line).unwrap();
        v.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn g1_send_initiated_key_order_and_roundtrip() {
        let p = Payload::SendInitiated {
            send_id: "71234-1781241549123-0".to_string(),
            verb: "send:pty".to_string(),
            send_path: "idle".to_string(),
            content_sha256: sha256_hex(b"the message"),
            content_len: 11,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(b"the message")],
            chunk_sha256s_capped: false,
            transcript: Some("/home/u/.claude/projects/slug/abc.jsonl".to_string()),
            transcript_offset: Some(4096),
            // ADD-20: None here → omitted, so the EXISTING pinned key order is
            // preserved (additive-only, CR-1). The present-preview key order is
            // pinned by `g1_content_preview_appended_last_in_key_order`.
            content_preview: None,
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        // EXACT serialized key order (envelope, event, payload §2.3.1 order).
        assert_eq!(
            keys_of(&line),
            vec![
                "v",
                "ts",
                "pid",
                "seq",
                "session",
                "name",
                "event",
                "send_id",
                "verb",
                "send_path",
                "content_sha256",
                "content_len",
                "chunks",
                "chunk_sha256s",
                "transcript",
                "transcript_offset"
            ]
        );
        // capped=false → the field is OMITTED (§2.3). content_preview None →
        // OMITTED too (additive default).
        assert!(!line.contains("chunk_sha256s_capped"));
        assert!(!line.contains("content_preview"));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], json!("send-initiated"));
        assert_eq!(v["content_len"], json!(11));
    }

    #[test]
    fn g1_representative_of_each_kind_roundtrips() {
        let anchor = Anchor {
            transcript: "/t.jsonl".into(),
            start_offset: 10,
            line_index: 2,
        };
        let kinds: Vec<(Payload, &str)> = vec![
            (
                Payload::ChunksDelivered {
                    send_id: "s".into(),
                    chunks_acked: 3,
                    ack_source: "input-sent".into(),
                },
                "chunks-delivered",
            ),
            (
                Payload::TurnAnchored {
                    send_id: "s".into(),
                    content_sha256: sha256_hex(b"x"),
                    anchor: anchor.clone(),
                    recovered: false,
                    attribution: None,
                },
                "turn-anchored",
            ),
            (
                Payload::TurnAnchoredMismatch {
                    send_id: "s".into(),
                    expected_sha: sha256_hex(b"x"),
                    actual_sha: sha256_hex(b"y"),
                    expected_len: 10,
                    actual_len: 4,
                    recovered: false,
                    attribution: None,
                },
                "turn-anchored-mismatch",
            ),
            (
                Payload::AnchorTimeout {
                    send_id: "s".into(),
                    waited_ms: 30000,
                },
                "anchor-timeout",
            ),
            (
                Payload::PendingAbandoned {
                    send_id: "s".into(),
                    reason: "session-died".into(),
                    recovered: None,
                    attribution: None,
                },
                "pending-abandoned",
            ),
            (
                Payload::ComposerCleared {
                    send_id: "s".into(),
                },
                "composer-cleared",
            ),
            (
                Payload::PrimingReadinessTimeout {
                    waited_ms: 5000,
                    phase: "pid-file".into(),
                },
                "priming-readiness-timeout",
            ),
            (
                Payload::StatusTransition {
                    status: "busy".into(),
                    source: "status-file-poll".into(),
                },
                "status-transition",
            ),
            (Payload::EventsTruncated, "events-truncated"),
        ];
        for (p, name) in kinds {
            let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["event"], json!(name), "tag for {name}");
            // Envelope always leads.
            let keys = keys_of(&line);
            assert_eq!(&keys[..4], &["v", "ts", "pid", "seq"]);
            // recovered/attribution OMITTED when false/None (§2.3).
            assert!(
                !line.contains("\"recovered\""),
                "{name} omits recovered:false"
            );
        }
    }

    #[test]
    fn g1_turn_anchored_recovered_includes_flags() {
        let p = Payload::TurnAnchored {
            send_id: "s".into(),
            content_sha256: sha256_hex(b"x"),
            anchor: Anchor {
                transcript: "/t.jsonl".into(),
                start_offset: 1,
                line_index: 0,
            },
            recovered: true,
            attribution: Some("offset".into()),
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["recovered"], json!(true));
        assert_eq!(v["attribution"], json!("offset"));
        assert_eq!(v["anchor"]["start_offset"], json!(1));
    }

    /// The R1 worst-case `send-initiated` line at an arbitrary sha-array size:
    /// longest realistic transcript path (~150 chars), 24-char name, max-width
    /// pid/seq/lens, `n_shas` entries serialized with a cap that admits them all.
    /// Shared by the G1 bound row AND the M-1 mutation-evidence twin (which runs
    /// it at 56 to prove the bound row REDs at the pre-red-team cap).
    fn worst_case_line(n_shas: usize) -> String {
        let path: String = format!(
            "/home/u/.claude/projects/{}/{}.jsonl",
            "a".repeat(80),
            "b".repeat(40)
        );
        assert!(
            path.len() >= 145,
            "path is the ~150-char worst case: {}",
            path.len()
        );
        let name = "n".repeat(24);
        let shas: Vec<String> = (0..n_shas).map(|i| sha256_hex(&[i as u8])).collect();
        let env = Envelope {
            v: 1,
            ts: "2026-06-06T06:09:00.123Z".to_string(),
            pid: 4_000_000_000, // max-width pid
            seq: u64::MAX,
            session: Some("11111111-2222-3333-4444-555555555555".to_string()),
            name: Some(name),
            start_ms: None,
        };
        let p = Payload::SendInitiated {
            send_id: format!("{}-1781241549123-{}", 4_000_000_000u64, u64::MAX),
            verb: "send:pty".to_string(),
            send_path: "busy-queued".to_string(),
            content_sha256: sha256_hex(b"big"),
            content_len: u64::MAX,
            chunks: n_shas as u32,
            chunk_sha256s: shas,
            chunk_sha256s_capped: false,
            transcript: Some(path),
            transcript_offset: Some(u64::MAX),
            // Preview-FREE worst case: this row pins the sha-only bound (the
            // existing G1 + me_g1 arithmetic). The preview-bearing worst case (and
            // its preview-first shrink) is a SEPARATE row,
            // `g1_worst_case_preview_shrinks_shas_survive` (ADD-20 §6.3 i).
            content_preview: None,
        };
        build_record_line(&env, &p, n_shas)
    }

    #[test]
    fn g1_worst_case_length_under_4096() {
        // R1 worst-case row: longest realistic transcript path (~150 chars) +
        // 24-char name + 48-entry sha array MUST serialize < 4096 bytes.
        //
        // MUTATION ARM (§11 G1): raising CHUNK_SHA_CAP to 56 REDs this row —
        // proving the bound is TEST-enforced, not arithmetic-asserted. The
        // COMMITTED form of that arm is `me_g1_cap_56_overflows_the_bound`
        // (mutation-evidence feature; merge-ruling M-1).
        let line = worst_case_line(CHUNK_SHA_CAP);
        assert!(
            line.len() + 1 < MAX_RECORD_BYTES,
            "worst-case line {}B must be < {MAX_RECORD_BYTES}B (CHUNK_SHA_CAP={CHUNK_SHA_CAP})",
            line.len() + 1
        );
    }

    /// The R1 worst-case `send-initiated` PAYLOAD (not line) carrying a
    /// `content_preview` body of `preview_bytes` (filler chars that survive
    /// redaction — short plain words). Shared by the ADD-20 §6.3 G1 rows and the
    /// me_add20 mutation evidence. Same envelope/path/name worst case as
    /// `worst_case_line`.
    fn worst_case_payload(n_shas: usize, preview_bytes: usize) -> (Envelope, Payload) {
        let path: String = format!(
            "/home/u/.claude/projects/{}/{}.jsonl",
            "a".repeat(80),
            "b".repeat(40)
        );
        let name = "n".repeat(24);
        let shas: Vec<String> = (0..n_shas).map(|i| sha256_hex(&[i as u8])).collect();
        let env = Envelope {
            v: 1,
            ts: "2026-06-06T06:09:00.123Z".to_string(),
            pid: 4_000_000_000,
            seq: u64::MAX,
            session: Some("11111111-2222-3333-4444-555555555555".to_string()),
            name: Some(name),
            start_ms: None,
        };
        // A preview body of short plain words (no run ≥24, no key prefix) so the
        // preview survives redaction VERBATIM at its full length — the worst case
        // for the line bound (a redacted body would be SHORTER).
        let preview = "ab ".repeat(preview_bytes.div_ceil(3));
        let preview = preview[..preview_bytes.min(preview.len())].to_string();
        let p = Payload::SendInitiated {
            send_id: format!("{}-1781241549123-{}", 4_000_000_000u64, u64::MAX),
            verb: "send:pty".to_string(),
            send_path: "busy-queued".to_string(),
            content_sha256: sha256_hex(b"big"),
            content_len: u64::MAX,
            chunks: n_shas as u32,
            chunk_sha256s: shas,
            chunk_sha256s_capped: false,
            transcript: Some(path),
            transcript_offset: Some(u64::MAX),
            content_preview: Some(preview),
        };
        (env, p)
    }

    #[test]
    fn g1_worst_case_preview_shrinks_shas_survive() {
        // ADD-20 §6.3 (i) WORST-CASE row: 48 shas + a 256B preview. The raw line
        // OVERFLOWS (corrected R4 arithmetic), so fit_line's belt must SHRINK the
        // preview (preview-first) and keep ALL 48 shas intact, landing < 4096.
        let (env, p) = worst_case_payload(CHUNK_SHA_CAP, PREVIEW_CAP_BYTES);
        // Precondition: the un-shrunk line really does overflow (else the row is
        // vacuous — it would not exercise the belt).
        let raw = build_record_line(&env, &p, CHUNK_SHA_CAP);
        assert!(
            raw.len() >= MAX_RECORD_BYTES,
            "the 48-sha + 256B-preview worst case must overflow un-shrunk \
             (raw line {}B ≥ {MAX_RECORD_BYTES}B bound) — else the shrink row is vacuous",
            raw.len()
        );
        // The belt output.
        let writer = EventWriter::new(
            tempdir().unwrap().path().join("s.events.jsonl"),
            env.session.clone(),
            env.name.clone(),
        );
        let line = writer.fit_line(&env, &p);
        // The O_APPEND atomic contract: line + '\n' ≤ the bound ⟺ line.len() < bound.
        assert!(
            line.len() < MAX_RECORD_BYTES,
            "belt must land ≤ {MAX_RECORD_BYTES}B incl. newline (line {}B)",
            line.len()
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        // ALL 48 shas survive — shas are NEVER sacrificed before the preview is gone.
        assert_eq!(
            v["chunk_sha256s"].as_array().unwrap().len(),
            CHUNK_SHA_CAP,
            "all {CHUNK_SHA_CAP} shas survive the preview-first shrink"
        );
        assert!(
            v.get("chunk_sha256s_capped").is_none(),
            "no sha was dropped → the capped flag is absent"
        );
        // The preview SHRANK (it is present-but-shorter OR fully omitted) — the
        // shrink demonstrably happened.
        let preview_len = v
            .get("content_preview")
            .and_then(|x| x.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        assert!(
            preview_len < PREVIEW_CAP_BYTES,
            "the preview was SHRUNK below the 256B cap (got {preview_len}B) — \
             the belt sacrificed preview, not shas"
        );
    }

    #[test]
    fn g1_typical_case_preview_survives_in_full() {
        // ADD-20 §6.3 (ii) TYPICAL-CASE row: ≤3 shas + a 256B preview → the
        // preview survives IN FULL (the ruling's "short messages effectively full"
        // where real records live). No shrink, line < 4096.
        let (env, p) = worst_case_payload(3, PREVIEW_CAP_BYTES);
        let writer = EventWriter::new(
            tempdir().unwrap().path().join("s.events.jsonl"),
            env.session.clone(),
            env.name.clone(),
        );
        let line = writer.fit_line(&env, &p);
        assert!(line.len() < MAX_RECORD_BYTES);
        let v: Value = serde_json::from_str(&line).unwrap();
        // The preview is present at its FULL 256B (no shrink happened).
        let preview = v["content_preview"].as_str().expect("preview present");
        assert_eq!(
            preview.len(),
            PREVIEW_CAP_BYTES,
            "the typical-case preview survives in full ({}B)",
            preview.len()
        );
        assert_eq!(v["chunk_sha256s"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn g1_content_preview_appended_last_in_key_order() {
        // ADD-20 §6.2: when present, content_preview is LAST in the pinned key
        // order (additive — the None case keeps the legacy order, pinned by
        // `g1_send_initiated_key_order_and_roundtrip`).
        let p = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(b"x"),
            content_len: 3,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(b"abc")],
            chunk_sha256s_capped: false,
            transcript: Some("/t.jsonl".into()),
            transcript_offset: Some(0),
            content_preview: Some("hello world".into()),
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        assert_eq!(
            keys_of(&line),
            vec![
                "v",
                "ts",
                "pid",
                "seq",
                "session",
                "name",
                "event",
                "send_id",
                "verb",
                "send_path",
                "content_sha256",
                "content_len",
                "chunks",
                "chunk_sha256s",
                "transcript",
                "transcript_offset",
                "content_preview",
            ]
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["content_preview"], json!("hello world"));
    }

    #[test]
    fn g1_over_cap_payload_caps_at_48_and_marks() {
        // A 49+-chunk payload: chunk_sha256s caps at CHUNK_SHA_CAP, sets the flag.
        let shas: Vec<String> = (0..60u32).map(|i| sha256_hex(&i.to_le_bytes())).collect();
        let p = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(b"x"),
            content_len: 60_000,
            chunks: 60,
            chunk_sha256s: shas,
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            content_preview: None,
        };
        let line = build_record_line(&env_for(0), &p, CHUNK_SHA_CAP);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["chunk_sha256s"].as_array().unwrap().len(), CHUNK_SHA_CAP);
        assert_eq!(v["chunk_sha256s_capped"], json!(true));
        // chunks COUNT stays the true 60 (it is not the array length).
        assert_eq!(v["chunks"], json!(60));
    }

    #[test]
    fn g1_shrink_to_fit_never_drops_record() {
        // A pathologically long path forces the shrink belt to drop sha entries;
        // the record is STILL emitted (never skipped — R1).
        let writer = EventWriter::new(
            tempdir().unwrap().path().join("s.events.jsonl"),
            Some("sid".into()),
            None,
        );
        let huge_path = format!("/{}", "p".repeat(3500)); // forces shrink-to-fit
        let shas: Vec<String> = (0..CHUNK_SHA_CAP).map(|i| sha256_hex(&[i as u8])).collect();
        let p = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(b"x"),
            content_len: 50_000,
            chunks: CHUNK_SHA_CAP as u32,
            chunk_sha256s: shas,
            chunk_sha256s_capped: false,
            transcript: Some(huge_path),
            transcript_offset: Some(0),
            content_preview: None,
        };
        let clock = FixedClock(1_781_241_549_123);
        writer.emit(&clock, &p).unwrap();
        let text = std::fs::read_to_string(writer.path()).unwrap();
        let r = parse_events(&text);
        // The record landed (NOT skipped) and every line is < 4096B.
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].event, "send-initiated");
        for line in text.lines() {
            // line + its trailing '\n' must fit the bound.
            assert!(line.len() < MAX_RECORD_BYTES, "shrunk line fits");
        }
        // The belt set the capped flag (it dropped sha entries to fit).
        assert_eq!(r.records[0].obj["chunk_sha256s_capped"], json!(true));
    }

    // ---------------------------------------------------------------------
    // G2: multi-process writer + torn-tail + interior-corrupt
    //
    // The re-exec trick (G2 §11): the parent spawns two children of the test
    // binary with QD_EVENTS_TEST_CHILD=<pidtag> set; each child appends N=200
    // records to the SAME tempdir file, then the parent asserts all 400 parse and
    // each pid's seq is 0..N-1 gapless monotonic. The child entrypoint is the
    // `events_test_child` test below, gated on the env var.
    // ---------------------------------------------------------------------

    const G2_CHILD_VAR: &str = "QD_EVENTS_TEST_CHILD";
    const G2_FILE_VAR: &str = "QD_EVENTS_TEST_FILE";
    const G2_N: u64 = 200;

    #[test]
    fn events_test_child() {
        // When the env var is set this "test" is actually the child entrypoint:
        // append N records to the shared file and exit. When unset it is a no-op
        // (so the normal `cargo test` run just sees it pass).
        let Ok(_tag) = std::env::var(G2_CHILD_VAR) else {
            return;
        };
        let file = std::env::var(G2_FILE_VAR).expect("child needs the file path");
        let writer = EventWriter::new(PathBuf::from(file), Some("sid".into()), None);
        let clock = FixedClock(1_781_241_549_123);
        for i in 0..G2_N {
            // A small, fixed payload (composer-cleared) — keeps the line short.
            writer
                .emit(
                    &clock,
                    &Payload::ComposerCleared {
                        send_id: format!("s{i}"),
                    },
                )
                .unwrap();
        }
    }

    #[test]
    fn g2_multi_process_writer_gapless_per_pid_seq() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("shared.events.jsonl");
        let exe = std::env::current_exe().unwrap();

        // Spawn two child processes of the test binary, each running ONLY the
        // child entrypoint test (filtered) with the env flags set.
        let mut kids = Vec::new();
        for tag in ["A", "B"] {
            let child = std::process::Command::new(&exe)
                .args(["--exact", "events::tests::events_test_child", "--nocapture"])
                .env(G2_CHILD_VAR, tag)
                .env(G2_FILE_VAR, &file)
                // Don't recurse into the harness's own child-spawning test.
                .env_remove("RUST_TEST_THREADS")
                .spawn()
                .expect("spawn child");
            kids.push(child);
        }
        for mut k in kids {
            let status = k.wait().expect("child wait");
            assert!(status.success(), "child exited non-zero");
        }

        // All 2N records parse; each pid's seq is 0..N-1 gapless monotonic.
        let text = std::fs::read_to_string(&file).unwrap();
        let r = parse_events(&text);
        assert_eq!(r.records.len() as u64, 2 * G2_N, "all 400 records parse");
        let pids: std::collections::HashSet<u32> = r.records.iter().map(|x| x.pid).collect();
        assert_eq!(pids.len(), 2, "two distinct writer pids");
        for pid in pids {
            let ordered = per_pid_ordered(&r.records, pid);
            assert_eq!(ordered.len() as u64, G2_N, "N records for pid {pid}");
            for (i, rec) in ordered.iter().enumerate() {
                assert_eq!(rec.seq, i as u64, "gapless monotonic seq for pid {pid}");
            }
        }
    }

    #[test]
    fn g2_torn_tail_skipped_silently() {
        // A complete record + a partial (no newline, half a record) → the reader
        // skips the torn tail SILENTLY and still yields the complete record.
        let whole = build_record_line(
            &env_for(0),
            &Payload::ComposerCleared {
                send_id: "s".into(),
            },
            CHUNK_SHA_CAP,
        );
        let text = format!("{whole}\n{{\"v\":1,\"ts\":\"2026-06-06T06:09\",\"pid\":7,\"se");
        let r = parse_events(&text);
        assert_eq!(r.records.len(), 1, "complete record yielded");
        assert_eq!(r.corrupt_interior, 0, "torn tail is NOT counted (silent)");
    }

    #[test]
    fn g2_interior_corrupt_counted_not_verdict_bearing() {
        // A corrupt line MID-file is counted (forensic) but the surrounding
        // complete records still parse — it is never verdict-bearing.
        let a = build_record_line(
            &env_for(0),
            &Payload::ComposerCleared {
                send_id: "a".into(),
            },
            CHUNK_SHA_CAP,
        );
        let b = build_record_line(
            &env_for(1),
            &Payload::ComposerCleared {
                send_id: "b".into(),
            },
            CHUNK_SHA_CAP,
        );
        let text = format!("{a}\n{{garbage not json}}\n{b}\n");
        let r = parse_events(&text);
        assert_eq!(r.records.len(), 2, "both complete records parse");
        assert_eq!(r.corrupt_interior, 1, "interior corruption counted");
    }

    /// §2.2 schema_version evolution / forward-compat (merge-ruling m-2, fixed
    /// in-window): an UNKNOWN event kind from a future writer (a) parses without
    /// being counted as corruption, (b) is NEVER terminal (no verdict impact),
    /// (c) does not disturb the real terminal's first-terminal-wins resolution —
    /// readers "skip unknown event names silently".
    #[test]
    fn unknown_event_kind_is_skipped_for_verdicts_not_corruption() {
        let known = build_record_line(
            &env_for(1),
            &Payload::AnchorTimeout {
                send_id: "s-1".into(),
                waited_ms: 5,
            },
            CHUNK_SHA_CAP,
        );
        // A v2-era record with an unknown kind + an unknown field, same send_id.
        let future = "{\"v\":2,\"ts\":\"2026-06-06T06:09:00.123Z\",\"pid\":71234,\
                      \"seq\":0,\"session\":\"11111111-2222-3333-4444-555555555555\",\
                      \"event\":\"telemetry-flush\",\"send_id\":\"s-1\",\"novel_field\":true}";
        let text = format!("{future}\n{known}\n");
        let r = parse_events(&text);
        // (a) parses, not corruption.
        assert_eq!(r.records.len(), 2, "unknown kind still parses as a record");
        assert_eq!(r.corrupt_interior, 0, "unknown kind is NOT corruption");
        // (b) never terminal.
        assert!(!is_terminal("telemetry-flush"));
        // (c) the REAL terminal resolves; the unknown (earlier in file order)
        // does not win first-terminal-wins.
        let term = first_terminal_for(&r.records, "s-1").expect("terminal found");
        assert_eq!(term.event, "anchor-timeout");
    }

    /// I5 (back-compat / one-way) — an OLD-format log (a pre-change `send-initiated`
    /// + `chunks-delivered`, no new kinds) interleaved with the THREE NEW kinds
    /// (`relay-delivered`, `message-seen`, `seen-failed`) all fold cleanly: every
    /// line parses, NONE is counted as corruption, and a reader that does not expect
    /// the new kinds (an OLD frame) is unaffected. The new on-disk lines are produced
    /// by the REAL build_record_line (the same path the emitters use), so this is
    /// the record-level half of I5; the relay-WIRE byte-identity half is proven by
    /// relay_server_differential/parity (no wire bytes change — events go ONLY into
    /// the local log, the one-way invariant).
    #[test]
    fn i5_back_compat_old_format_and_new_kinds_fold_together() {
        // An OLD-format trace (no new kinds) — exactly what a pre-change dispatch wrote.
        let old_si = build_record_line(
            &env_for(0),
            &Payload::SendInitiated {
                send_id: "relay-100-1".into(),
                verb: "send:relay".into(),
                send_path: "relay".into(),
                content_sha256: sha256_hex(b"hi"),
                content_len: 2,
                chunks: 1,
                chunk_sha256s: vec![sha256_hex(b"hi")],
                chunk_sha256s_capped: false,
                transcript: None,
                transcript_offset: None,
                content_preview: None,
            },
            CHUNK_SHA_CAP,
        );
        // The THREE NEW kinds (a NEW dispatch writing into the same log an OLD reader tails).
        let rd = build_record_line(
            &env_for(1),
            &Payload::RelayDelivered {
                send_id: "relay-100-1".into(),
                content_sha256: sha256_hex(b"hi"),
            },
            CHUNK_SHA_CAP,
        );
        let ms = build_record_line(
            &env_for(2),
            &Payload::MessageSeen {
                send_id: "relay-100-1".into(),
                content_sha256: sha256_hex(b"hi"),
            },
            CHUNK_SHA_CAP,
        );
        let sf = build_record_line(
            &env_for(3),
            &Payload::SeenFailed {
                send_id: "relay-200-2".into(),
                reason: "recipient-gone".into(),
            },
            CHUNK_SHA_CAP,
        );
        let text = format!("{old_si}\n{rd}\n{ms}\n{sf}\n");
        let r = parse_events(&text);
        assert_eq!(r.records.len(), 4, "every old+new line parses as a record");
        assert_eq!(
            r.corrupt_interior, 0,
            "no new kind is treated as corruption"
        );
        // The first terminal for the relay send is message-seen (relay-delivered is
        // non-terminal and earlier in file order — it must NOT win).
        let term = first_terminal_for(&r.records, "relay-100-1").expect("terminal");
        assert_eq!(
            term.event, "message-seen",
            "message-seen wins over the non-terminal"
        );
        // seen-failed resolves as the terminal for its own send_id.
        let t2 = first_terminal_for(&r.records, "relay-200-2").expect("terminal");
        assert_eq!(t2.event, "seen-failed");
    }

    // ---------------------------------------------------------------------
    // §4.3 rotation
    // ---------------------------------------------------------------------

    #[test]
    fn rotation_reserve_band_takes_terminal_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Plant a file just into the reserve band (cap < size <= cap+reserve).
        let filler = vec![b'x'; (EVENTS_MAX_BYTES + 1024) as usize];
        std::fs::write(&path, &filler).unwrap();
        let writer = EventWriter::new(path.clone(), Some("s".into()), None);
        let clock = FixedClock(1_781_241_549_123);
        // A non-terminal record is DROPPED (Err).
        assert!(writer
            .emit(
                &clock,
                &Payload::ComposerCleared {
                    send_id: "s".into()
                }
            )
            .is_err());
        // A terminal record is ACCEPTED (+ the events-truncated marker).
        writer
            .emit(
                &clock,
                &Payload::PendingAbandoned {
                    send_id: "s".into(),
                    reason: "session-died".into(),
                    recovered: None,
                    attribution: None,
                },
            )
            .unwrap();
        // A SECOND terminal write in the band must NOT re-emit the marker (lead
        // review fix: marker once per file; per-write re-emission would eat the
        // reserve with markers).
        writer
            .emit(
                &clock,
                &Payload::AnchorTimeout {
                    send_id: "s2".into(),
                    waited_ms: 1,
                },
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("pending-abandoned"), "terminal record landed");
        assert!(text.contains("anchor-timeout"), "second terminal landed");
        assert_eq!(
            text.matches("\"event\":\"events-truncated\"").count(),
            1,
            "marker appended EXACTLY once across multiple band writes"
        );
        // Per-pid seq order agrees with file order (the marker takes the earlier
        // seq — lead review fix for the marker/record seq inversion).
        let tail = &text[filler.len()..];
        let r = parse_events(tail);
        let seqs: Vec<u64> = r.records.iter().map(|x| x.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "file order == per-pid seq order");
    }

    // ---------------------------------------------------------------------
    // §4.4 reader ordering / merge
    // ---------------------------------------------------------------------

    #[test]
    fn reader_merges_sessionid_and_byname_files() {
        let dir = tempdir().unwrap();
        let state = dir.path();
        let sid_writer = EventWriter::for_key(state, "sid-1", Some("sid-1".into()), None);
        let byname_writer =
            EventWriter::for_key(state, &byname_key("alpha"), None, Some("alpha".into()));
        let clock = FixedClock(1_781_241_549_123);
        sid_writer
            .emit(
                &clock,
                &Payload::ComposerCleared {
                    send_id: "a".into(),
                },
            )
            .unwrap();
        byname_writer
            .emit(
                &clock,
                &Payload::ComposerCleared {
                    send_id: "b".into(),
                },
            )
            .unwrap();
        let merged = read_merged(state, Some("sid-1"), Some("alpha"));
        assert_eq!(merged.records.len(), 2, "both files merged");
    }

    // ---------------------------------------------------------------------
    // G4: terminal-set / cheap-event trap
    // ---------------------------------------------------------------------

    /// A deps fake for await/recovery that never resolves a transcript (no
    /// recovery) and sleeps instantly.
    struct NoRecoveryDeps {
        now: AtomicI64,
    }
    impl RecoveryDeps for NoRecoveryDeps {
        fn read_transcript(&self, _path: &str) -> Option<String> {
            None
        }
        fn resolve_transcript(&self, _s: Option<&str>, _n: Option<&str>) -> Option<String> {
            None
        }
        fn now_ms(&self) -> i64 {
            self.now.load(Ordering::SeqCst)
        }
    }
    impl AwaitDeps for NoRecoveryDeps {
        fn sleep(&self, _ms: u64) {}
    }

    #[test]
    fn g4_cheap_events_never_return_success() {
        // A stream of ONLY send-initiated + chunks-delivered + composer-cleared +
        // status-transition (NO terminal). await_received must NOT return a
        // success variant — it exhausts the budget and emits anchor-timeout.
        //
        // MUTATION ARM (§11 G4): adding chunks-delivered to first_terminal_for's
        // is_terminal gate (or to TERMINAL_EVENTS) makes this RED — it would
        // return Anchored/early on a cheap event.
        let dir = tempdir().unwrap();
        let state = dir.path();
        let writer = EventWriter::for_key(state, "sid-1", Some("sid-1".into()), None);
        // Use OUR OWN (live) pid so the dead-dangling rule never fires.
        let clock = FixedClock(1_000_000);
        let cheap = [
            Payload::SendInitiated {
                send_id: "s".into(),
                verb: "send:pty".into(),
                send_path: "idle".into(),
                content_sha256: sha256_hex(b"x"),
                content_len: 1,
                chunks: 1,
                chunk_sha256s: vec![sha256_hex(b"x")],
                chunk_sha256s_capped: false,
                transcript: None,
                transcript_offset: None,
                content_preview: None,
            },
            Payload::ChunksDelivered {
                send_id: "s".into(),
                chunks_acked: 1,
                ack_source: "input-sent".into(),
            },
            Payload::ComposerCleared {
                send_id: "s".into(),
            },
            Payload::StatusTransition {
                status: "busy".into(),
                source: "status-file-poll".into(),
            },
        ];
        for p in &cheap {
            writer.emit(&clock, p).unwrap();
        }
        let deps = NoRecoveryDeps {
            now: AtomicI64::new(1_000_000),
        };
        let budget = AwaitBudget {
            poll_ms: 1,
            max_polls: 3,
        };
        let ctx = ReaderCtx {
            state_dir: state,
            session_id: Some("sid-1"),
            name: None,
        };
        let got = await_received(&deps, &clock, &writer, ctx, "s", budget);
        // NEVER a success — AnchorTimeout (emitted) is the only allowed result.
        assert_eq!(got, Received::AnchorTimeout);
        assert!(
            !matches!(got, Received::Anchored | Received::AnchoredMismatch),
            "cheap events must never satisfy the wait"
        );
        // It EMITTED a positive anchor-timeout (§8).
        let merged = read_merged(state, Some("sid-1"), None);
        assert!(
            merged.records.iter().any(|r| r.event == "anchor-timeout"),
            "budget exhaustion emits a positive anchor-timeout"
        );
    }

    // ---------------------------------------------------------------------
    // G5: recovery-read planted transcript states
    // ---------------------------------------------------------------------

    /// A RecoveryDeps backed by a single planted transcript text.
    struct PlantedDeps {
        text: Option<String>,
        path: String,
        now: i64,
    }
    impl RecoveryDeps for PlantedDeps {
        fn read_transcript(&self, _path: &str) -> Option<String> {
            self.text.clone()
        }
        fn resolve_transcript(&self, _s: Option<&str>, _n: Option<&str>) -> Option<String> {
            self.text.as_ref().map(|_| self.path.clone())
        }
        fn now_ms(&self) -> i64 {
            self.now
        }
    }

    /// Build a user-record JSONL line (the transcript shape user_record_text reads).
    fn user_line(text: &str, ts: Option<&str>) -> String {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), json!("user"));
        o.insert("message".into(), json!({ "content": text }));
        if let Some(t) = ts {
            o.insert("timestamp".into(), json!(t));
        }
        Value::Object(o).to_string()
    }

    fn si_record(
        content: &str,
        chunk_shas: Vec<String>,
        transcript: Option<&str>,
        offset: Option<u64>,
        ts: &str,
    ) -> EventRecord {
        let p = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(content.as_bytes()),
            content_len: content.len() as u64,
            chunks: chunk_shas.len() as u32,
            chunk_sha256s: chunk_shas,
            chunk_sha256s_capped: false,
            transcript: transcript.map(str::to_string),
            transcript_offset: offset,
            content_preview: None,
        };
        let env = Envelope {
            v: 1,
            ts: ts.to_string(),
            pid: std::process::id(),
            seq: 0,
            session: Some("sid".into()),
            name: None,
            start_ms: None,
        };
        let line = build_record_line(&env, &p, CHUNK_SHA_CAP);
        parse_one(&line).unwrap()
    }

    #[test]
    fn g5_exact_match_anchored_offset() {
        let msg = "hello world this is the message";
        let transcript = format!("{}\n", user_line(msg, None));
        let si = si_record(
            msg,
            vec![sha256_hex(msg.as_bytes())],
            Some("/t.jsonl"),
            Some(0),
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        let v = recovery_read(&deps, &si);
        match v {
            RecoveryVerdict::Anchored { attribution, .. } => {
                assert_eq!(attribution, "offset");
            }
            other => panic!("expected Anchored, got {other:?}"),
        }
    }

    #[test]
    fn g5_chunk_prefix_truncated_at_boundary_and_midchunk() {
        // Build a >1-chunk message; truncate at a chunk boundary AND mid-chunk.
        let full = "A".repeat(CHUNK_BYTES * 2 + 500); // 3 chunks (2 full + partial)
        let chunks = chunk_text(&full, CHUNK_BYTES);
        let chunk_shas: Vec<String> = chunks.iter().map(|c| sha256_hex(c.as_bytes())).collect();

        // (a) truncate at the first chunk boundary (exactly CHUNK_BYTES landed).
        let trunc_boundary = "A".repeat(CHUNK_BYTES);
        // (b) truncate mid-second-chunk (CHUNK_BYTES + 300 landed).
        let trunc_mid = "A".repeat(CHUNK_BYTES + 300);

        for landed in [trunc_boundary, trunc_mid] {
            let transcript = format!("{}\n", user_line(&landed, None));
            let si = si_record(
                &full,
                chunk_shas.clone(),
                Some("/t.jsonl"),
                Some(0),
                "2026-06-06T06:00:00.000Z",
            );
            let deps = PlantedDeps {
                text: Some(transcript),
                path: "/t.jsonl".into(),
                now: 0,
            };
            match recovery_read(&deps, &si) {
                RecoveryVerdict::Truncated {
                    expected_len,
                    actual_len,
                    ..
                } => {
                    assert_eq!(expected_len, full.len() as u64);
                    assert_eq!(actual_len, landed.len() as u64);
                }
                other => panic!(
                    "expected Truncated for landed {}, got {other:?}",
                    landed.len()
                ),
            }
        }
    }

    #[test]
    fn g5_absent_abandoned() {
        // (c) SEARCHED-no-match: a NON-matching candidate exists past the anchor →
        // the disclosed Abandoned closer, carrying the search attribution ("offset").
        let transcript = format!("{}\n", user_line("a completely different message", None));
        let si = si_record(
            "the original message",
            vec![sha256_hex(b"the original message")],
            Some("/t.jsonl"),
            Some(0),
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        assert_eq!(
            recovery_read(&deps, &si),
            RecoveryVerdict::Abandoned {
                attribution: "offset".into()
            }
        );
    }

    #[test]
    fn g5_identical_resend_with_offset_post_offset_wins() {
        // Same content twice; with an offset past the FIRST copy, recovery anchors
        // on the SECOND occurrence (offset window excludes the earlier line).
        let msg = "duplicate message body";
        let first = user_line(msg, None);
        let second = user_line(msg, None);
        let transcript = format!("{first}\n{second}\n");
        // Offset = just past the first line → only the second is in-window.
        let offset = (first.len() + 1) as u64;
        let si = si_record(
            msg,
            vec![sha256_hex(msg.as_bytes())],
            Some("/t.jsonl"),
            Some(offset),
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        match recovery_read(&deps, &si) {
            RecoveryVerdict::Anchored { anchor, .. } => {
                // The anchored line starts at/after the offset (the second copy).
                assert!(anchor.start_offset >= offset, "post-offset occurrence wins");
            }
            other => panic!("expected Anchored on the second copy, got {other:?}"),
        }
    }

    #[test]
    fn g5_offset_absent_pre_send_timestamp_excluded() {
        // Offset-absent path: a candidate timestamped BEFORE the send is EXCLUDED (no
        // false-Anchor on an identical earlier copy — R6). With the only record
        // excluded, the searched window is EMPTY → (b) EmptyWindow → NO terminal (the
        // recipient hasn't demonstrably progressed past the send; still growable), NOT
        // a foreclosing Abandoned.
        let msg = "identical content";
        // Earlier copy (before send) + no later copy.
        let earlier = user_line(msg, Some("2026-06-06T05:00:00.000Z"));
        let transcript = format!("{earlier}\n");
        let si = si_record(
            msg,
            vec![sha256_hex(msg.as_bytes())],
            None,                       // no transcript path → re-resolve + time-window
            None,                       // no offset
            "2026-06-06T06:00:00.000Z", // send ts is AFTER the earlier copy
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        // The earlier copy is excluded → empty searched window → EmptyWindow (no terminal).
        assert_eq!(recovery_read(&deps, &si), RecoveryVerdict::EmptyWindow);
    }

    #[test]
    fn g5_offset_absent_verdict_carries_time_window_attribution() {
        let msg = "post-send message";
        // A copy AFTER the send ts → in-window, anchors with time-window attr.
        let later = user_line(msg, Some("2026-06-06T06:00:01.000Z"));
        let transcript = format!("{later}\n");
        let si = si_record(
            msg,
            vec![sha256_hex(msg.as_bytes())],
            None,
            None,
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        match recovery_read(&deps, &si) {
            RecoveryVerdict::Anchored { attribution, .. } => {
                assert_eq!(attribution, "time-window");
            }
            other => panic!("expected time-window Anchored, got {other:?}"),
        }
    }

    #[test]
    fn g5_idempotence_no_verdict_flip() {
        // Re-running recovery_read on the same inputs yields the SAME verdict.
        let msg = "stable message";
        let transcript = format!("{}\n", user_line(msg, None));
        let si = si_record(
            msg,
            vec![sha256_hex(msg.as_bytes())],
            Some("/t.jsonl"),
            Some(0),
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        let v1 = recovery_read(&deps, &si);
        let v2 = recovery_read(&deps, &si);
        assert_eq!(v1, v2, "idempotent: no verdict flip on re-run");
    }

    #[test]
    fn g5_emit_recovery_verdict_idempotent_append() {
        // emit_recovery_verdict re-checks the dangling predicate before appending:
        // a second call (terminal already present) does NOT append a second one.
        let dir = tempdir().unwrap();
        let state = dir.path();
        let msg = "anchor me";
        let writer = EventWriter::for_key(state, "sid", Some("sid".into()), None);
        let clock = FixedClock(1_000_000);
        // Plant the send-initiated in the file.
        let si_payload = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(msg.as_bytes()),
            content_len: msg.len() as u64,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(msg.as_bytes())],
            chunk_sha256s_capped: false,
            transcript: Some("/t.jsonl".into()),
            transcript_offset: Some(0),
            content_preview: None,
        };
        writer.emit(&clock, &si_payload).unwrap();
        let merged = read_merged(state, Some("sid"), None);
        let si = send_initiated_for(&merged.records, "s").unwrap();
        let deps = PlantedDeps {
            text: Some(format!("{}\n", user_line(msg, None))),
            path: "/t.jsonl".into(),
            now: 0,
        };
        let ctx = ReaderCtx {
            state_dir: state,
            session_id: Some("sid"),
            name: None,
        };
        emit_recovery_verdict(&deps, &writer, &clock, ctx, &si).unwrap();
        emit_recovery_verdict(&deps, &writer, &clock, ctx, &si).unwrap();
        // Exactly ONE turn-anchored terminal exists (the second call short-circuited).
        let after = read_merged(state, Some("sid"), None);
        let anchored = after
            .records
            .iter()
            .filter(|r| r.event == "turn-anchored")
            .count();
        assert_eq!(anchored, 1, "idempotent append: one terminal only");
    }

    #[test]
    fn g5_foreign_record_yields_abandoned_not_false_anchor() {
        // Negative control: a foreign record (different content) sits past the anchor
        // → (c) SEARCHED-no-match → disclosed Abandoned (attribution "offset"), NOT a
        // false-Anchor.
        let transcript = format!("{}\n", user_line("totally unrelated text here", None));
        let si = si_record(
            "our actual message",
            vec![sha256_hex(b"our actual message")],
            Some("/t.jsonl"),
            Some(0),
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(transcript),
            path: "/t.jsonl".into(),
            now: 0,
        };
        assert_eq!(
            recovery_read(&deps, &si),
            RecoveryVerdict::Abandoned {
                attribution: "offset".into()
            }
        );
    }

    #[test]
    fn g5_source_unavailable_when_transcript_unreadable() {
        // (a) build_window None (read/resolve failure) → SourceUnavailable (NO terminal),
        // NOT Abandoned. text:None → read_transcript AND resolve_transcript both yield
        // None, exercising BOTH build_window arms.
        let msg = "the send whose transcript we cannot read";
        let sha = vec![sha256_hex(msg.as_bytes())];
        let deps = PlantedDeps {
            text: None,
            path: "/gone.jsonl".into(),
            now: 0,
        };
        // offset-PRESENT read-failure arm.
        let si_offset = si_record(
            msg,
            sha.clone(),
            Some("/gone.jsonl"),
            Some(0),
            "2026-06-06T06:00:00.000Z",
        );
        assert_eq!(
            recovery_read(&deps, &si_offset),
            RecoveryVerdict::SourceUnavailable,
            "offset-present read-failure → SourceUnavailable, not Abandoned"
        );
        // offset-ABSENT resolve-failure arm.
        let si_absent = si_record(msg, sha, None, None, "2026-06-06T06:00:00.000Z");
        assert_eq!(
            recovery_read(&deps, &si_absent),
            RecoveryVerdict::SourceUnavailable,
            "offset-absent resolve-failure → SourceUnavailable, not Abandoned"
        );
    }

    #[test]
    fn g5_empty_window_no_terminal() {
        // (b) read SUCCEEDED but ZERO candidate records past the anchor → EmptyWindow
        // (NO terminal) — still growable, NOT the foreclosing Abandoned. A readable but
        // empty transcript, offset 0.
        let si = si_record(
            "landed later, not yet flushed",
            vec![sha256_hex(b"landed later, not yet flushed")],
            Some("/t.jsonl"),
            Some(0),
            "2026-06-06T06:00:00.000Z",
        );
        let deps = PlantedDeps {
            text: Some(String::new()),
            path: "/t.jsonl".into(),
            now: 0,
        };
        assert_eq!(recovery_read(&deps, &si), RecoveryVerdict::EmptyWindow);
    }

    #[test]
    fn g5_missing_content_sha_unattributable() {
        // (d) a send-initiated lacking content_sha256 can never be searched →
        // Unattributable (a search that never ran; never "no-candidate").
        let line = r#"{"v":1,"ts":"2026-06-06T06:00:00.000Z","pid":1,"seq":0,"session":"sid","event":"send-initiated","send_id":"s","verb":"send:pty","transcript":"/t.jsonl","transcript_offset":0}"#;
        let si = parse_one(line).unwrap();
        let deps = PlantedDeps {
            text: Some(format!("{}\n", user_line("anything", None))),
            path: "/t.jsonl".into(),
            now: 0,
        };
        assert_eq!(recovery_read(&deps, &si), RecoveryVerdict::Unattributable);
    }

    #[test]
    fn g5_recovery_event_lattice_disclosure_and_no_terminal() {
        // (c) discloses recovered:true + attribution; (d) unattributable, NO flags;
        // (a)/(b) mint NO terminal.
        match recovery_event(
            "s",
            "sha",
            &RecoveryVerdict::Abandoned {
                attribution: "offset".into(),
            },
        )
        .expect("(c) mints a terminal")
        {
            Payload::PendingAbandoned {
                reason,
                recovered,
                attribution,
                ..
            } => {
                assert_eq!(reason, "recovery-no-candidate");
                assert_eq!(recovered, Some(true));
                assert_eq!(attribution.as_deref(), Some("offset"));
            }
            other => panic!("expected disclosed PendingAbandoned, got {other:?}"),
        }
        match recovery_event("s", "sha", &RecoveryVerdict::Unattributable)
            .expect("(d) mints a terminal")
        {
            Payload::PendingAbandoned {
                reason,
                recovered,
                attribution,
                ..
            } => {
                assert_eq!(reason, "recovery-unattributable");
                assert_eq!(recovered, None);
                assert!(attribution.is_none());
            }
            other => panic!("expected unattributable PendingAbandoned, got {other:?}"),
        }
        assert!(recovery_event("s", "sha", &RecoveryVerdict::SourceUnavailable).is_none());
        assert!(recovery_event("s", "sha", &RecoveryVerdict::EmptyWindow).is_none());
    }

    // ---------------------------------------------------------------------
    // G6: dead-writer rule (dead pid resolves via recovery; live pid stays dangling)
    // ---------------------------------------------------------------------

    /// Spawn-and-reap a child to obtain a known-DEAD pid (it has exited but we
    /// hold its pid). `true /bin/true`-style: spawn `/bin/sh -c exit 0`, wait.
    fn known_dead_pid() -> u32 {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn reapable child");
        let pid = child.id();
        let mut child = child;
        let _ = child.wait();
        pid
    }

    /// Build a send-initiated record with an explicit pid + ts (for the §7 rule).
    fn si_record_with_pid(pid: u32, ts: &str, transcript: &str, msg: &str) -> EventRecord {
        let p = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(msg.as_bytes()),
            content_len: msg.len() as u64,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(msg.as_bytes())],
            chunk_sha256s_capped: false,
            transcript: Some(transcript.to_string()),
            transcript_offset: Some(0),
            content_preview: None,
        };
        let env = Envelope {
            v: 1,
            ts: ts.to_string(),
            pid,
            seq: 0,
            session: Some("sid".into()),
            name: None,
            start_ms: None,
        };
        parse_one(&build_record_line(&env, &p, CHUNK_SHA_CAP)).unwrap()
    }

    #[test]
    fn g6_dead_pid_dangling_resolves_via_recovery() {
        let dead = known_dead_pid();
        let msg = "recover me after death";
        // send ts well in the past so age > 30s under the virtual now.
        let send_ts = "2026-06-06T06:00:00.000Z";
        let now = iso_to_epoch_ms(send_ts).unwrap() + 60_000; // +60s → age>30s
        let si = si_record_with_pid(dead, send_ts, "/t.jsonl", msg);
        let records = vec![si.clone()];
        assert!(
            is_dead_dangling(&records, &si, now),
            "dead pid + age>30s + no terminal → dead-dangling"
        );

        // await_received over a file containing ONLY this dead-pid dangling send
        // resolves via inline recovery (the transcript anchors it). We write the
        // dead-pid send-initiated record directly (the envelope pid the §7 rule
        // reads must be the DEAD one — the live writer would stamp our own pid).
        let dir = tempdir().unwrap();
        let state = dir.path();
        let writer = EventWriter::for_key(state, "sid", Some("sid".into()), None);
        let clock = FixedClock(now);
        let dead_line = build_record_line(
            &Envelope {
                v: 1,
                ts: send_ts.to_string(),
                pid: dead,
                seq: 0,
                session: Some("sid".into()),
                name: None,
                start_ms: None,
            },
            &Payload::SendInitiated {
                send_id: "s2".into(),
                verb: "send:pty".into(),
                send_path: "idle".into(),
                content_sha256: sha256_hex(msg.as_bytes()),
                content_len: msg.len() as u64,
                chunks: 1,
                chunk_sha256s: vec![sha256_hex(msg.as_bytes())],
                chunk_sha256s_capped: false,
                transcript: Some("/t.jsonl".into()),
                transcript_offset: Some(0),
                content_preview: None,
            },
            CHUNK_SHA_CAP,
        );
        append_record(writer.path(), &dead_line).unwrap();

        let deps = PlantedDeps {
            text: Some(format!("{}\n", user_line(msg, None))),
            path: "/t.jsonl".into(),
            now,
        };
        let budget = AwaitBudget {
            poll_ms: 1,
            max_polls: 2,
        };
        let ctx = ReaderCtx {
            state_dir: state,
            session_id: Some("sid"),
            name: None,
        };
        let got = await_received(&DeadDangAwait(deps), &clock, &writer, ctx, "s2", budget);
        assert_eq!(
            got,
            Received::Anchored,
            "dead-dangling resolved via recovery"
        );
    }

    #[test]
    fn g6_live_pid_dangling_not_resolved_control() {
        // Control: our OWN live pid → NOT dead-dangling → stays dangling.
        let msg = "still alive";
        let send_ts = "2026-06-06T06:00:00.000Z";
        let now = iso_to_epoch_ms(send_ts).unwrap() + 60_000;
        let si = si_record_with_pid(std::process::id(), send_ts, "/t.jsonl", msg);
        let records = vec![si.clone()];
        assert!(
            !is_dead_dangling(&records, &si, now),
            "live pid → NOT dead-dangling (rule requires pid dead)"
        );
    }

    /// Build a send-initiated record with an explicit pid + ts + envelope start_ms
    /// (the RF-6 dead-writer arm subject).
    fn si_record_with_pid_start(
        pid: u32,
        start_ms: Option<i64>,
        ts: &str,
        transcript: &str,
        msg: &str,
    ) -> EventRecord {
        let p = Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(msg.as_bytes()),
            content_len: msg.len() as u64,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(msg.as_bytes())],
            chunk_sha256s_capped: false,
            transcript: Some(transcript.to_string()),
            transcript_offset: Some(0),
            content_preview: None,
        };
        let env = Envelope {
            v: 1,
            ts: ts.to_string(),
            pid,
            seq: 0,
            session: Some("sid".into()),
            name: None,
            start_ms,
        };
        parse_one(&build_record_line(&env, &p, CHUNK_SHA_CAP)).unwrap()
    }

    /// Spawn a long-lived FOREIGN child (not our pid) whose `proc_start_ms` is
    /// readable; returns the child handle + its pid. Reap with `kill` + `wait`.
    fn spawn_live_foreign_child() -> (std::process::Child, u32) {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id();
        (child, pid)
    }

    /// RF-6 (R3d) — the start_ms arm closes the v1 imperfection: a recycled pid (a
    /// FOREIGN process is ALIVE on the pid, but the record's recorded `start_ms` is
    /// far in the PAST, so the live start has drifted beyond START_TIME_SLACK_MS) no
    /// longer SUPPRESSES the dead-dangling trigger. v1 (bare pid-alive) returned
    /// false here forever — the named imperfection.
    #[test]
    fn rf6_recycled_pid_does_not_suppress_dead_dangling() {
        let (mut child, pid) = spawn_live_foreign_child();
        let msg = "writer died, its pid got recycled by a stranger";
        let send_ts = "2026-06-06T06:00:00.000Z";
        let now = iso_to_epoch_ms(send_ts).unwrap() + 60_000; // age > 30s
        // The foreign pid is alive, but the recorded start_ms = 1000 (epoch ms in
        // 1970): the live process's real start is ~1.7e12, drifting WAY beyond the
        // 2-min slack → recycled-pid → the original writer is gone.
        let si = si_record_with_pid_start(pid, Some(1000), send_ts, "/t.jsonl", msg);
        let records = vec![si.clone()];
        let verdict = is_dead_dangling(&records, &si, now);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            verdict,
            "a FOREIGN live pid whose recorded start_ms proves it is a DIFFERENT \
             incarnation must NOT suppress the dead-dangling trigger (RF-6 start_ms arm)"
        );
    }

    /// RF-6 control (non-vacuity of the arm): the SAME incarnation (the FOREIGN child
    /// is alive AND the record's start_ms == its real current start) is correctly NOT
    /// dead-dangling — the arm fires ONLY on a genuine incarnation drift, never on a
    /// matching one. Distinct verdict from the recycled case above (verdict-inequality
    /// on the same live foreign pid, toggled only by start_ms).
    #[test]
    fn rf6_matching_incarnation_is_not_dead_dangling() {
        let (mut child, pid) = spawn_live_foreign_child();
        // The child's REAL current start (within slack of itself by construction).
        let child_start = crate::effects::proc_start_ms(pid as i32);
        let msg = "same incarnation, genuine writer";
        let send_ts = "2026-06-06T06:00:00.000Z";
        let now = iso_to_epoch_ms(send_ts).unwrap() + 60_000;
        let si = si_record_with_pid_start(pid, child_start, send_ts, "/t.jsonl", msg);
        let records = vec![si.clone()];
        let verdict = is_dead_dangling(&records, &si, now);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !verdict,
            "a foreign live pid with a MATCHING recorded start_ms is the SAME writer → \
             NOT dead-dangling (the arm must not over-fire on the genuine original writer)"
        );
    }

    // ---------------------------------------------------------------------
    // R3d — recovery-ladder forensics: an episode is reconstructable from the
    // event log ALONE (emit the four kinds, read the file back, replay).
    // ---------------------------------------------------------------------

    #[test]
    fn r3d_recovery_episode_reconstructs_from_log_alone() {
        let dir = tempdir().unwrap();
        let state = dir.path();
        let sid = "11111111-2222-3333-4444-555555555555";
        let writer = EventWriter::for_key(state, sid, Some(sid.into()), None);
        let clock = FixedClock(1_781_241_549_123);

        // A wedged session climbs the ladder: rung 1 entered+timeout → rung 2
        // entered+timeout → rung 3 entered+timeout → rung 4 entered → succeeded.
        let episode = vec![
            LadderEvent::RungEntered { session_id: sid.into(), rung: "pidfd-signal".into() },
            LadderEvent::RungTimeout { session_id: sid.into(), rung: "pidfd-signal".into(), waited_ms: 5_000 },
            LadderEvent::RungEntered { session_id: sid.into(), rung: "control-wake".into() },
            LadderEvent::RungTimeout { session_id: sid.into(), rung: "control-wake".into(), waited_ms: 10_000 },
            LadderEvent::RungEntered { session_id: sid.into(), rung: "pty-inject".into() },
            LadderEvent::RungTimeout { session_id: sid.into(), rung: "pty-inject".into(), waited_ms: 15_000 },
            LadderEvent::RungEntered { session_id: sid.into(), rung: "respawn".into() },
            LadderEvent::RungSucceeded { session_id: sid.into(), rung: "respawn".into() },
        ];
        for e in &episode {
            emit_ladder_event(&writer, &clock, e).expect("emit ladder event");
        }

        // Read the file back from disk and replay — NO in-memory state.
        let read = read_merged(state, Some(sid), None);
        let replayed = replay_recovery_episode(&read.records);
        assert_eq!(
            replayed, episode,
            "the recovery episode must reconstruct byte-for-event from the log alone"
        );
        // Every emitted record is ≤ MAX_RECORD_BYTES (the O_APPEND atomic contract).
        let text = std::fs::read_to_string(writer.path()).unwrap();
        for line in text.lines() {
            assert!(
                line.len() < MAX_RECORD_BYTES,
                "ladder record must stay under the {MAX_RECORD_BYTES}B append bound"
            );
        }
    }

    #[test]
    fn r3d_recovery_crit_episode_reconstructs_from_log() {
        let dir = tempdir().unwrap();
        let state = dir.path();
        let sid = "deadbeef-0000-1111-2222-333344445555";
        let writer = EventWriter::for_key(state, sid, Some(sid.into()), None);
        let clock = FixedClock(1_781_241_549_123);
        // Three confirmed failures → CRIT (the terminal episode).
        let episode = vec![
            LadderEvent::RungEntered { session_id: sid.into(), rung: "respawn".into() },
            LadderEvent::RungTimeout { session_id: sid.into(), rung: "respawn".into(), waited_ms: 96_000 },
            LadderEvent::Crit { session_id: sid.into(), consecutive_failures: 3 },
        ];
        for e in &episode {
            emit_ladder_event(&writer, &clock, e).expect("emit");
        }
        let read = read_merged(state, Some(sid), None);
        assert_eq!(replay_recovery_episode(&read.records), episode);
        // The recovery-ladder kinds are NON-terminal in the SEND sense (no send_id;
        // never satisfy a delivery wait — the cheap-event trap stays closed).
        for kind in ["rung-entered", "rung-succeeded", "rung-timeout", "recovery-crit"] {
            assert!(!is_terminal(kind), "{kind} must NOT be a delivery terminal");
        }
    }

    // A small AwaitDeps adapter wrapping PlantedDeps (recovery only fires on the
    // dead-dangling path; sleep is instant).
    struct DeadDangAwait(PlantedDeps);
    impl RecoveryDeps for DeadDangAwait {
        fn read_transcript(&self, p: &str) -> Option<String> {
            self.0.read_transcript(p)
        }
        fn resolve_transcript(&self, s: Option<&str>, n: Option<&str>) -> Option<String> {
            self.0.resolve_transcript(s, n)
        }
        fn now_ms(&self) -> i64 {
            self.0.now_ms()
        }
    }
    impl AwaitDeps for DeadDangAwait {
        fn sleep(&self, _ms: u64) {}
    }

    // ---------------------------------------------------------------------
    // iso round-trip (the ts math underpinning age/exclusion)
    // ---------------------------------------------------------------------

    #[test]
    fn iso_epoch_roundtrip() {
        for ms in [
            0i64,
            1_717_530_000_000,
            1_781_241_549_123,
            1_709_209_845_678,
        ] {
            let iso = epoch_ms_to_iso(ms);
            assert_eq!(iso_to_epoch_ms(&iso), Some(ms), "roundtrip {iso}");
        }
    }

    // ---------------------------------------------------------------------
    // §9 / rev C row 24 — WatchGuard (the exit-finalizer; M2/M3 surface)
    // ---------------------------------------------------------------------

    /// Read the event names in file order from a writer's events file.
    fn event_names(path: &std::path::Path) -> Vec<String> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        parse_events(&text)
            .records
            .iter()
            .map(|r| r.event.clone())
            .collect()
    }

    #[test]
    fn watchguard_disarm_emits_nothing() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1_781_241_549_123);
        let writer = EventWriter::for_key(
            dir.path(),
            "sid-watch",
            Some("sid-watch".to_string()),
            Some("alpha".to_string()),
        );
        {
            let g = WatchGuard::arm(&writer, &clock, "71234-1-0");
            g.disarm(); // the watch reached its own terminal — NO Drop emission.
        }
        // Disarmed → the file was never even created (no record written).
        assert!(event_names(writer.path()).is_empty());
    }

    #[test]
    fn watchguard_drop_armed_emits_pending_abandoned() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1_781_241_549_123);
        let writer = EventWriter::for_key(
            dir.path(),
            "sid-watch",
            Some("sid-watch".to_string()),
            Some("alpha".to_string()),
        );
        {
            // Armed + dropped WITHOUT disarm (the early-return / panic-unwind path).
            let _g = WatchGuard::arm(&writer, &clock, "71234-1-0");
        }
        // Drop emitted pending-abandoned{watch-interrupted} — the terminal that
        // keeps a watched send from silently vanishing.
        let names = event_names(writer.path());
        assert_eq!(names, vec!["pending-abandoned".to_string()]);
        // It is a TERMINAL kind (G4: the watch's drop verdict satisfies await).
        let text = std::fs::read_to_string(writer.path()).unwrap();
        let recs = parse_events(&text).records;
        assert_eq!(
            recs[0].str_field("reason").as_deref(),
            Some("watch-interrupted")
        );
        assert!(is_terminal(&recs[0].event));
        assert_eq!(recs[0].send_id().as_deref(), Some("71234-1-0"));
    }

    #[test]
    fn warn_emit_is_best_effort_writes_record() {
        // warn_emit is the M2/M3 emit wrapper: on success it writes the record.
        let dir = tempdir().unwrap();
        let clock = FixedClock(1_781_241_549_123);
        let writer = EventWriter::for_key(
            dir.path(),
            "sid-w",
            Some("sid-w".to_string()),
            Some("alpha".to_string()),
        );
        warn_emit(
            &writer,
            &clock,
            &Payload::ChunksDelivered {
                send_id: "71234-1-0".to_string(),
                chunks_acked: 2,
                ack_source: "input-sent".to_string(),
            },
        );
        let names = event_names(writer.path());
        assert_eq!(names, vec!["chunks-delivered".to_string()]);
        // chunks-delivered is NON-terminal (the cheap-event trap stays closed).
        assert!(!is_terminal("chunks-delivered"));
    }

    // =======================================================================
    // M-1 MUTATION EVIDENCE (merge ruling, CR-3 reproducible-by-command) —
    // feature-gated like tests/negative_control.rs. Each test PASSES by proving
    // the corresponding gate row's assert WOULD FAIL under the named mutation —
    // the committed, re-runnable form of the gate report's "live-fired → RED"
    // claims. Run:
    //
    //   scripts/build-lock.sh cargo test -p quorum-dispatch --features mutation-evidence
    //
    // The two claims needing PRIVATE access (build_record_line) live here; the
    // stream-shape claims live in tests/ack2_mutation_evidence.rs.
    // =======================================================================

    /// M-1 claim 1 (G1 cap): at the pre-red-team cap of 56, the SAME worst-case
    /// construction the G1 bound row uses EXCEEDS the 4096B O_APPEND bound — so
    /// raising CHUNK_SHA_CAP to 56 REDs `g1_worst_case_length_under_4096`. The
    /// red-team R1 finding, permanently auditable.
    #[cfg(feature = "mutation-evidence")]
    #[test]
    fn me_g1_cap_56_overflows_the_bound() {
        let mutated = worst_case_line(56);
        assert!(
            mutated.len() + 1 >= MAX_RECORD_BYTES,
            "cap=56 worst case must overflow the {MAX_RECORD_BYTES}B bound \
             (got {}B) — i.e. the G1 row REDs under the mutation",
            mutated.len() + 1
        );
        // Control: the real cap fits (the G1 row is green un-mutated).
        let real = worst_case_line(CHUNK_SHA_CAP);
        assert!(real.len() + 1 < MAX_RECORD_BYTES);
    }

    /// ADD-20 §6.3 ORC RIDER — the worst-case preview arithmetic lands as a
    /// MEASURED assert (not a comment). At the 48-sha worst case the sha-only line
    /// is ≈3870B with a margin of ≈226B to the 4096B bound; the `content_preview`
    /// key+quotes overhead is ~21B, leaving ~205B for the preview BODY. We measure
    /// the REAL serialized lengths and assert the band: the sha-only worst case
    /// leaves a tight (but positive) margin, the field overhead is ~21B, and the
    /// fit_line belt therefore admits a preview body in the ~150-230B range — i.e.
    /// the full 256B does NOT survive at the 48-sha worst case (the shrink is
    /// real), but a substantial preview does (the field is not gutted to nothing).
    #[cfg(feature = "mutation-evidence")]
    #[test]
    fn me_add20_worst_case_preview_arithmetic() {
        let sha_only = worst_case_line(CHUNK_SHA_CAP);
        let margin = MAX_RECORD_BYTES - (sha_only.len() + 1);
        // The measured sha-only worst case + margin (R4-corrected ≈3870B / ≈226B).
        assert!(
            (3800..=3950).contains(&(sha_only.len() + 1)),
            "sha-only worst case measured {}B (expected ≈3870B band)",
            sha_only.len() + 1
        );
        assert!(
            (150..=300).contains(&margin),
            "margin measured {margin}B (expected ≈226B band)"
        );
        // The content_preview field overhead = the key+quoting cost MINUS the body.
        // An empty body is OMITTED (insert_opt_str drops ""), so we measure with a
        // 1-byte body (`,"content_preview":"x"` over the sha-only line) and subtract
        // the 1 body byte to get the pure `,"content_preview":""` overhead.
        let (env, base) = worst_case_payload(CHUNK_SHA_CAP, 0);
        let p_one = with_preview(&base, Some("x".to_string()));
        let with_one = build_record_line(&env, &p_one, CHUNK_SHA_CAP);
        let overhead = (with_one.len() as i64 - 1) - sha_only.len() as i64;
        assert!(
            (15..=30).contains(&overhead),
            "content_preview field overhead measured {overhead}B (expected ~21B)"
        );
        // The belt-admitted preview body at the 48-sha worst case: SHRUNK below the
        // 256B cap (the shrink is real) but a substantial body survives.
        let writer = EventWriter::new(
            tempdir().unwrap().path().join("s.events.jsonl"),
            env.session.clone(),
            env.name.clone(),
        );
        let (_, p_full) = worst_case_payload(CHUNK_SHA_CAP, PREVIEW_CAP_BYTES);
        let line = writer.fit_line(&env, &p_full);
        let v: Value = serde_json::from_str(&line).unwrap();
        let body = v
            .get("content_preview")
            .and_then(|x| x.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        assert!(line.len() < MAX_RECORD_BYTES);
        assert_eq!(
            v["chunk_sha256s"].as_array().unwrap().len(),
            CHUNK_SHA_CAP,
            "all 48 shas survive (preview yields first)"
        );
        assert!(
            body < PREVIEW_CAP_BYTES,
            "the 256B preview was SHRUNK at the worst case (admitted {body}B)"
        );
        assert!(
            body >= 150,
            "a substantial preview body still survives (~150-230B; got {body}B) — \
             the belt sacrifices preview to the margin, not to nothing"
        );
    }

    /// ADD-20 §6.3 (iii) / §3.1 ORDER mutation: a SHA-FIRST shrink shape FAILS the
    /// order predicate. The committed proof that the belt sacrifices the preview
    /// before ANY sha — a shape that dropped a sha while keeping the full preview
    /// violates the order and must be detectable as a violation.
    #[cfg(feature = "mutation-evidence")]
    #[test]
    fn me_add20_sha_first_shrink_shape_fails_the_order() {
        let (env, p_full) = worst_case_payload(CHUNK_SHA_CAP, PREVIEW_CAP_BYTES);
        let writer = EventWriter::new(
            tempdir().unwrap().path().join("s.events.jsonl"),
            env.session.clone(),
            env.name.clone(),
        );
        // The REAL belt output (preview-first): all shas survive, preview shrunk.
        let real_line = writer.fit_line(&env, &p_full);
        let real: Value = serde_json::from_str(&real_line).unwrap();
        // The ORDER predicate (§6.3): the shape is well-ordered iff NO sha was
        // dropped while a full-cap preview survived — i.e. if the preview is below
        // the cap OR all shas are present. A sha-first mutation drops a sha while
        // keeping the full preview.
        let well_ordered = |v: &Value| -> bool {
            let shas = v["chunk_sha256s"].as_array().map(|a| a.len()).unwrap_or(0);
            let preview = v
                .get("content_preview")
                .and_then(|x| x.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            // shas dropped (< CHUNK_SHA_CAP) is ONLY allowed once the preview is
            // gone (preview must be fully sacrificed first).
            shas == CHUNK_SHA_CAP || preview == 0
        };
        assert!(well_ordered(&real), "the real belt output is well-ordered");
        // The MUTATION: a sha-first shape — drop 5 shas but KEEP the full 256B
        // preview. This is the shape the WRONG order would produce.
        let mut mutated = real.clone();
        mutated["chunk_sha256s"] =
            Value::Array(real["chunk_sha256s"].as_array().unwrap()[..CHUNK_SHA_CAP - 5].to_vec());
        // give it the FULL preview body (≥150B → > 0) to model "kept preview".
        mutated["content_preview"] = Value::String("x".repeat(200));
        assert!(
            !well_ordered(&mutated),
            "the sha-first shrink shape FAILS the order predicate — proving the \
             belt MUST sacrifice the preview before any sha"
        );
    }

    /// M-1 claim 2 (G4 terminal-set pollution): a cheap-event-only stream has NO
    /// terminal under the real [`is_terminal`], but WOULD have one under a set
    /// polluted with "chunks-delivered" — the G4 trap row's green hinges exactly
    /// on the const's content (adding chunks-delivered REDs it).
    #[cfg(feature = "mutation-evidence")]
    #[test]
    fn me_g4_terminal_set_pollution_would_flip_the_trap() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1_781_241_549_123);
        let writer = EventWriter::for_key(dir.path(), "sid-me4", Some("sid-me4".into()), None);
        for p in [
            Payload::SendInitiated {
                send_id: "s".into(),
                verb: "send:pty".into(),
                send_path: "idle".into(),
                content_sha256: sha256_hex(b"m"),
                content_len: 1,
                chunks: 1,
                chunk_sha256s: vec![sha256_hex(b"m")],
                chunk_sha256s_capped: false,
                transcript: None,
                transcript_offset: None,
                content_preview: None,
            },
            Payload::ChunksDelivered {
                send_id: "s".into(),
                chunks_acked: 1,
                ack_source: "input-sent".into(),
            },
            Payload::ComposerCleared {
                send_id: "s".into(),
            },
            Payload::StatusTransition {
                status: "busy".into(),
                source: "status-file-poll".into(),
            },
        ] {
            writer.emit(&clock, &p).unwrap();
        }
        let recs = parse_events(&std::fs::read_to_string(writer.path()).unwrap()).records;
        // Real set: NO terminal (the G4 row's green).
        assert!(first_terminal_for(&recs, "s").is_none());
        // Polluted set: the SAME stream yields a "terminal" → the row REDs.
        let polluted = |e: &str| is_terminal(e) || e == "chunks-delivered";
        assert!(
            recs.iter()
                .any(|r| polluted(&r.event) && r.send_id().as_deref() == Some("s")),
            "under the polluted set the cheap stream WOULD satisfy the wait — \
             proving G4 REDs on exactly that mutation"
        );
    }

    /// M-1 claim 5 (WatchGuard Drop deletion): the deletion's output shape (=
    /// what a disarmed guard produces: nothing) FAILS the row predicate that the
    /// armed-drop output satisfies — deleting the Drop emission REDs
    /// `watchguard_drop_armed_emits_pending_abandoned`.
    #[cfg(feature = "mutation-evidence")]
    #[test]
    fn me_watchguard_deletion_shape_fails_the_row_predicate() {
        let clock = FixedClock(1_781_241_549_123);
        // Armed-drop output (the real Drop emission).
        let dir_a = tempdir().unwrap();
        let w_a = EventWriter::for_key(dir_a.path(), "sid-a", Some("sid-a".into()), None);
        {
            let _g = WatchGuard::arm(&w_a, &clock, "id-1");
        }
        // Deletion-shaped output: a guard whose Drop emission is gone behaves
        // exactly like disarm (no record) — the committed stand-in for the
        // deleted `warn_emit` in `Drop`.
        let dir_b = tempdir().unwrap();
        let w_b = EventWriter::for_key(dir_b.path(), "sid-b", Some("sid-b".into()), None);
        {
            let g = WatchGuard::arm(&w_b, &clock, "id-1");
            g.disarm();
        }
        // The row predicate: the events file contains pending-abandoned.
        let row_predicate = |p: &Path| event_names(p).contains(&"pending-abandoned".to_string());
        assert!(
            row_predicate(w_a.path()),
            "armed-drop output satisfies the row"
        );
        assert!(
            !row_predicate(w_b.path()),
            "the deletion-shaped output FAILS the row predicate — deleting the \
             Drop emission REDs the WatchGuard row"
        );
    }

    // ---------------------------------------------------------------------
    // D1 (delivery contract): send-failed terminal (§C1) + reader-side handling
    // + queued honesty (§C4/§C2)
    // ---------------------------------------------------------------------

    #[test]
    fn d1_send_failed_is_terminal_and_queued_kinds_are_not() {
        // §C1: send-failed joins the terminal set (first-terminal-wins, rotation-
        // protected). §C4: the queued/non-terminal kinds stay NON-terminal — a
        // busy-queued initiation and its relay/chunk acks are never "landed".
        assert!(is_terminal("send-failed"), "send-failed is terminal (§C1)");
        assert!(TERMINAL_EVENTS.contains(&"send-failed"));
        for non in [
            "send-initiated",
            "chunks-delivered",
            "relay-delivered",
            "turn-accepted",
            "composer-cleared",
        ] {
            assert!(!is_terminal(non), "{non} must stay NON-terminal (queued is honest)");
        }
    }

    #[test]
    fn d2_turn_accepted_serializes_non_terminal_send_id_and_content_sha() {
        // C5/C3 (daemon-lane delivered): turn-accepted is NON-terminal and carries
        // send_id (the resident turn id) + content_sha256 (the correlation key),
        // same key order/shape as relay-delivered. It must never be conflated with
        // landed — only the terminal says landed.
        assert!(
            !is_terminal("turn-accepted"),
            "turn-accepted is NON-terminal (delivered, not landed)"
        );
        assert!(
            !TERMINAL_EVENTS.contains(&"turn-accepted"),
            "turn-accepted is not in the terminal set"
        );
        let dir = tempdir().unwrap();
        let state = dir.path();
        let writer = EventWriter::for_key(state, "sid", Some("sid".into()), None);
        let clock = FixedClock(1_000_000);
        writer
            .emit(
                &clock,
                &Payload::TurnAccepted {
                    send_id: "turn-7".into(),
                    content_sha256: sha256_hex(b"steer body"),
                },
            )
            .unwrap();
        let text = std::fs::read_to_string(events_path(state, "sid")).unwrap();
        let v: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["event"], "turn-accepted");
        assert_eq!(v["v"], 1);
        assert_eq!(v["send_id"], "turn-7");
        assert_eq!(v["content_sha256"], sha256_hex(b"steer body"));
        assert!(
            v.get("reason").is_none(),
            "turn-accepted carries no reason/prose"
        );
    }

    #[test]
    fn d1_send_failed_serializes_with_optional_send_id() {
        let dir = tempdir().unwrap();
        let state = dir.path();
        let writer = EventWriter::for_key(state, "sid", Some("sid".into()), None);
        let clock = FixedClock(1_000_000);
        // send_id OMITTED (the relay-door shape): content_sha256 + reason present.
        writer
            .emit(
                &clock,
                &Payload::SendFailed {
                    send_id: None,
                    content_sha256: sha256_hex(b"m"),
                    reason: "no-relay".into(),
                },
            )
            .unwrap();
        // send_id PRESENT (a door that already has one): included.
        writer
            .emit(
                &clock,
                &Payload::SendFailed {
                    send_id: Some("s1".into()),
                    content_sha256: sha256_hex(b"m"),
                    reason: "no-relay".into(),
                },
            )
            .unwrap();
        let text = std::fs::read_to_string(events_path(state, "sid")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["event"], "send-failed");
        assert!(
            lines[0].get("send_id").is_none(),
            "send_id OMITTED when None (§2.2 absent-never-null)"
        );
        assert_eq!(lines[0]["content_sha256"], sha256_hex(b"m"));
        assert_eq!(lines[0]["reason"], "no-relay");
        assert_eq!(lines[1]["send_id"], "s1");
    }

    #[test]
    fn d1_send_failed_maps_to_failure_not_catchall() {
        // received_from_terminal maps send-failed to an EXPLICIT failure (never the
        // unknown-terminal catch-all, never a success); verdict_from_terminal →
        // Abandoned (a door failure has no anchor).
        let env = Envelope {
            v: 1,
            ts: "2026-06-06T06:00:00.000Z".into(),
            pid: 1,
            seq: 0,
            session: Some("sid".into()),
            name: None,
            start_ms: None,
        };
        let p = Payload::SendFailed {
            send_id: Some("s".into()),
            content_sha256: sha256_hex(b"m"),
            reason: "no-relay".into(),
        };
        let rec = parse_one(&build_record_line(&env, &p, CHUNK_SHA_CAP)).unwrap();
        assert_eq!(
            received_from_terminal(&rec),
            Received::SendFailed {
                reason: "no-relay".into()
            }
        );
        assert_eq!(
            verdict_from_terminal(&rec),
            RecoveryVerdict::Abandoned {
                attribution: String::new()
            }
        );
    }

    #[test]
    fn d1_await_received_resolves_on_send_failed_never_hangs() {
        // A joinable send-failed (carries send_id) SATISFIES the await — the consumer
        // returns a failure immediately, never hanging for another terminal, never a
        // success. (The reader-sweep requirement, for the joinable door case.)
        let dir = tempdir().unwrap();
        let state = dir.path();
        let writer = EventWriter::for_key(state, "sid-sf", Some("sid-sf".into()), None);
        let clock = FixedClock(1_000_000);
        writer
            .emit(
                &clock,
                &Payload::SendInitiated {
                    send_id: "s".into(),
                    verb: "send:pty".into(),
                    send_path: "idle".into(),
                    content_sha256: sha256_hex(b"x"),
                    content_len: 1,
                    chunks: 1,
                    chunk_sha256s: vec![sha256_hex(b"x")],
                    chunk_sha256s_capped: false,
                    transcript: None,
                    transcript_offset: None,
                    content_preview: None,
                },
            )
            .unwrap();
        writer
            .emit(
                &clock,
                &Payload::SendFailed {
                    send_id: Some("s".into()),
                    content_sha256: sha256_hex(b"x"),
                    reason: "no-relay".into(),
                },
            )
            .unwrap();
        let deps = NoRecoveryDeps {
            now: AtomicI64::new(1_000_000),
        };
        let budget = AwaitBudget {
            poll_ms: 1,
            max_polls: 3,
        };
        let ctx = ReaderCtx {
            state_dir: state,
            session_id: Some("sid-sf"),
            name: None,
        };
        let got = await_received(&deps, &clock, &writer, ctx, "s", budget);
        assert_eq!(
            got,
            Received::SendFailed {
                reason: "no-relay".into()
            }
        );
        assert!(
            !matches!(got, Received::Anchored | Received::AnchoredMismatch),
            "a door failure must never read as success"
        );
    }

    #[test]
    fn d1_busy_queued_resolves_to_exactly_one_terminal() {
        // §C4/§C2: a busy-queued initiation is NON-terminal; the send then resolves to
        // EXACTLY ONE terminal (here the happy turn-anchored landing). The queued phase
        // is never itself presented as landed. (Post R5 seam ruling 01KX88WKGP the pty
        // partial-write door mints NO terminal — a busy-queued send that fails to fully
        // write stays dead-dangling and is closed by `qd delivery:recover`, proven in
        // tests/delivery_recover_verb.rs, not by a door terminal here.)
        let dir = tempdir().unwrap();
        let state = dir.path();
        let writer = EventWriter::for_key(state, "sid-q", Some("sid-q".into()), None);
        let clock = FixedClock(1_000_000);
        writer
            .emit(
                &clock,
                &Payload::SendInitiated {
                    send_id: "q".into(),
                    verb: "send:pty".into(),
                    send_path: "busy-queued".into(),
                    content_sha256: sha256_hex(b"x"),
                    content_len: 1,
                    chunks: 1,
                    chunk_sha256s: vec![sha256_hex(b"x")],
                    chunk_sha256s_capped: false,
                    transcript: None,
                    transcript_offset: None,
                    content_preview: None,
                },
            )
            .unwrap();
        // The queued initiation is NOT a terminal.
        let merged0 = read_merged(state, Some("sid-q"), None);
        assert!(
            first_terminal_for(&merged0.records, "q").is_none(),
            "busy-queued is non-terminal — never presented as delivered"
        );
        // Resolve with exactly one terminal (the happy landing).
        writer
            .emit(
                &clock,
                &Payload::TurnAnchored {
                    send_id: "q".into(),
                    content_sha256: sha256_hex(b"x"),
                    anchor: Anchor {
                        transcript: "/t".into(),
                        start_offset: 0,
                        line_index: 0,
                    },
                    recovered: false,
                    attribution: None,
                },
            )
            .unwrap();
        let merged1 = read_merged(state, Some("sid-q"), None);
        let terms: Vec<&EventRecord> = merged1
            .records
            .iter()
            .filter(|r| is_terminal(&r.event) && r.send_id().as_deref() == Some("q"))
            .collect();
        assert_eq!(
            terms.len(),
            1,
            "exactly one terminal for the queued send (C2)"
        );
        assert_eq!(terms[0].event, "turn-anchored");
    }
}
