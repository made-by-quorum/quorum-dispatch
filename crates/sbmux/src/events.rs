//! Daemon event stream (ACK-1, ack1-spec §1/§3) — an ADVISORY session-level
//! write-log.
//!
//! Standing declaration (rev C §2.1, carried verbatim): this stream carries NO
//! send_id (send_id never touches the wire — rev C §2.3) and is therefore not
//! joinable to specific sends in production; the authoritative landed-verdict
//! is ALWAYS the engine anchor (`turn-anchored`, ACK-2). Its value: forensics
//! ("did ANY bytes hit this PTY in the window?"), write-failure surfacing,
//! lifecycle truth. `pty-bytes-written` is NOT a receipt — the kernel may drop
//! bytes after a successful PTY write (ADD-6).
//!
//! Durability is **at-most-once** (no fsync-per-record): a daemon SIGKILL can
//! eat the tail of a file with NO gap signal — it looks like clean EOF.
//! Consumers must not infer "no write happened" from daemon-stream silence.
//! Cross-restart fencing lives in the FILENAME: `<session>.daemon.<epoch>.jsonl`
//! — a new owner never appends to a predecessor's file (epoch+1), so abrupt
//! respawn races cannot interleave two writers in one file. Readers concatenate
//! epochs in filename order.
//!
//! File location: `<resolved sbmux socket dir>/events/` — the SAME tier
//! resolution as `sbmux.sock` (socket.rs two-tier XDG/sbHome + the C1 override
//! seam), so events are discoverable wherever the socket is. Never literal
//! /tmp (ADD-14: the socket tiers have no /tmp tier).

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Daemon event stream schema version. ADDITIVE-ONLY evolution rule
/// (ack1-spec §3):
/// (a) new OPTIONAL fields land with #[serde(default)] semantics for readers;
/// (b) new event variants may be added (readers MUST skip unknown "event"
///     values, never error — see [`parse_line`]);
/// (c) fields are never renamed, retyped, or removed; reuse is banned;
/// (d) any additive change bumps this constant; consumers warn (not fail) on
///     newer-than-known versions;
/// (e) deny_unknown_fields is BANNED on all event types.
///
/// Version 2 (W2 / SPEC-v2 §5.A): `session-opened` MAY additionally carry the
/// agent-pid identity token `pid_start_ms` + `boot_id` (both OPTIONAL,
/// omit-when-None). The bump signals "a token *may* be present"; pre-token (v1)
/// lines still parse under the v2 reader (`#[serde(default)]`), and v2 lines
/// parse under a v1-shaped reader (`deny_unknown_fields` banned). See ADR 0019.
pub const DAEMON_EVENTS_SCHEMA_VERSION: u32 = 2;

/// Default per-file size cap (5 MiB). Overridable via `SBMUX_EVENTS_MAX_BYTES`
/// (test knob + ops escape hatch). After the cap, only TERMINAL-class records
/// are admitted, within [`TERMINAL_RESERVE_BYTES`].
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Reserve above the cap that admits TERMINAL-class records only
/// (`session-closed`, `events-truncated`) so the close bookend survives
/// rotation (rev C §2.2 / ledger row 26).
pub const TERMINAL_RESERVE_BYTES: u64 = 64 * 1024;

/// Common envelope, #[serde(flatten)]ed into every variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventMeta {
    /// Session name (validate_session_name-safe; also in the filename —
    /// duplicated so a record is self-describing when files are concatenated).
    pub session: String,
    /// Ownership epoch, u64 >= 1. Duplicates the filename fence token so a
    /// record stays self-describing off its filename.
    pub epoch: u64,
    /// Counts every ACCEPTED event incl. rotation-suppressed ones; starts at 1.
    /// In-file gaps after an events-truncated record = rotation drops (a
    /// positive signal); pre-cap files are gapless. Write-event seq order ==
    /// PTY write order (assigned under the pty_writer lock — client_handler
    /// SendInput site).
    pub seq: u64,
    /// Daemon wall clock, Unix epoch MILLISECONDS (u64). FORENSIC-ONLY:
    /// cross-file correlation is session identity + time window, never
    /// load-bearing (rev C §2.2).
    pub ts_ms: u64,
}

/// One record = one line of serde_json. JSON tag field is "event", kebab-case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum DaemonEvent {
    /// First record of every file/epoch (seq == 1), the open bookend.
    SessionOpened {
        #[serde(flatten)]
        meta: EventMeta,
        /// PTY child pid, captured ONCE at spawn (uncontended; never the
        /// try_lock-at-emit path). 0 only if the spawn API yielded no pid.
        pid: u32,
        /// DAEMON_EVENTS_SCHEMA_VERSION at write time. Carried ONLY here —
        /// per-file version stamp (first line of every file).
        schema_version: u32,
        /// Agent-pid identity token (W2 / SPEC-v2 §5.A): kernel start-time of
        /// the recorded `pid` (the PTY child / agent), epoch-MILLISECONDS,
        /// ms-floored on both platforms (N1/D1). `None` on read failure
        /// (fail-safe). ADDITIVE (rule (a)): `#[serde(default)]` so pre-token
        /// (v1) lines still deserialize under the v2 reader; `skip_serializing_if`
        /// keeps a token-absent line byte-stable with v1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid_start_ms: Option<u64>,
        /// Agent-pid identity token (W2): per-boot-stable OPAQUE id, compared by
        /// EXACT string equality only. `None` if unreadable (fail-safe). Pairs
        /// with `pid_start_ms` to defeat pid-reuse false-LIVE across reboots.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boot_id: Option<String>,
    },
    /// Bytes written+flushed to the PTY master at the SendInput site.
    /// NOT a receipt (ADD-6: kernel may still drop). Advisory.
    PtyBytesWritten {
        #[serde(flatten)]
        meta: EventMeta,
        /// COUNT of bytes the write call reported written (never payload).
        /// == content_len at this site today; kept distinct so future
        /// partial-write reporting is additive, not a reshape.
        bytes: u64,
        /// Lowercase hex (64 ASCII chars) SHA-256 of the EXACT SendInput
        /// `data` byte vector — no canonicalization, no trimming. Hex, NOT
        /// base64. Per-frame == per-chunk for chunked engine sends.
        content_sha256: String,
        /// Byte length of `data` (u64).
        content_len: u64,
    },
    /// The PTY write errored (honest failure; client also gets ServerMsg::Error).
    PtyWriteFailed {
        #[serde(flatten)]
        meta: EventMeta,
        /// Raw OS errno when the io::Error carries one; None otherwise
        /// (e.g. the poisoned-mutex synthetic error).
        errno: Option<i32>,
        /// Display string of the io::Error (diagnostic, not parsed). std
        /// io::Error Display never embeds the write buffer; R-PRIV asserts
        /// payload-absence over the whole file anyway.
        error: String,
        /// Join keys for the failed content — precision addition over rev C's
        /// `{errno}` sketch (orc-approved at checkpoint, ruling item 2): the
        /// test-level sha join needs the key on the failure row too; same
        /// privacy class as PtyBytesWritten, no new exposure.
        content_sha256: String,
        content_len: u64,
    },
    /// Close bookend. ABSENCE is expected on daemon SIGKILL (at-most-once);
    /// the next epoch's FILENAME is the respawn fence.
    SessionClosed {
        #[serde(flatten)]
        meta: EventMeta,
        reason: CloseReason,
    },
    /// Cap boundary marker (rotation). After this, only terminal-class
    /// records appear in this file; seq gaps after it = suppressed records.
    EventsTruncated {
        #[serde(flatten)]
        meta: EventMeta,
        /// The configured cap that was hit (bytes), for the reader's WARN.
        cap_bytes: u64,
    },
    /// Optional liveness tick, DEFAULT OFF (SBMUX_EVENTS_HEARTBEAT_MS).
    Heartbeat {
        #[serde(flatten)]
        meta: EventMeta,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum CloseReason {
    /// KillSession verb.
    Killed,
    /// Reaper found the PTY child dead (cleanup task).
    ChildExited,
    /// Graceful daemon shutdown drain.
    DaemonShutdown,
    /// Drop-belt fallback (an uninstrumented removal path; should not occur —
    /// its appearance in a file is itself a finding).
    Dropped,
}

/// Parse one JSONL line into a typed event. Returns `None` (never errors) on
/// unknown event variants or malformed lines — the forward-compat read rule
/// (evolution rule (b)): readers SKIP what they don't know.
pub fn parse_line(line: &str) -> Option<DaemonEvent> {
    serde_json::from_str::<DaemonEvent>(line).ok()
}

/// Daemon-level events context: resolved once at server start.
/// `None` from [`EventsCtx::from_env`] = emitter disabled (kill switch).
#[derive(Debug, Clone)]
pub struct EventsCtx {
    /// `<resolved socket dir>/events`.
    pub dir: PathBuf,
    /// Per-file cap (SBMUX_EVENTS_MAX_BYTES, default 5 MiB).
    pub max_bytes: u64,
}

impl EventsCtx {
    /// Build the context from the RESOLVED socket dir + process env.
    ///
    /// `SBMUX_EVENTS_DISABLED=1` → `None` (loud single warn). This kill switch
    /// is BOTH the operational escape hatch and the gate's wiring-level
    /// mutation lever (R-MUT m1): suppressing the emitter must turn the
    /// harness RED.
    pub fn from_env(socket_dir: &Path) -> Option<Self> {
        if std::env::var("SBMUX_EVENTS_DISABLED")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            tracing::warn!("SBMUX_EVENTS_DISABLED=1 — daemon event stream is OFF (advisory write-log will not be produced)");
            return None;
        }
        let max_bytes = std::env::var("SBMUX_EVENTS_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_BYTES);
        Some(Self {
            dir: socket_dir.join("events"),
            max_bytes,
        })
    }
}

/// Per-session event writer: owns one `<session>.daemon.<epoch>.jsonl` file.
///
/// Locking design (ack1-spec §2, red-team F2 pinned): seq counter, byte
/// accounting, truncation flag, and the fd live in ONE std::sync::Mutex —
/// cap check + seq assignment + append are a single atomic critical section.
/// At the SendInput site the emit happens while the caller still holds the
/// pty_writer lock (lock order pty→events at that site ONLY; every other emit
/// site takes the events lock alone; nothing ever takes pty after events ⇒
/// no deadlock) so write-event seq order == PTY byte order.
pub struct EventWriter {
    session: String,
    epoch: u64,
    /// Close once-flag, OUTSIDE the mutex and shared by construction (every
    /// close path — kill/reap/shutdown/Drop-belt — sees this same flag via
    /// the session's Arc<EventWriter>), excluding double close bookends
    /// (red-team F9).
    closed: AtomicBool,
    /// Emit-failure warn rate limiter (once per writer).
    warned: AtomicBool,
    inner: Mutex<Inner>,
}

struct Inner {
    file: File,
    seq: u64,
    bytes_written: u64,
    truncated: bool,
    max_bytes: u64,
}

impl EventWriter {
    /// Open the next-epoch file for `session` and emit the `session-opened`
    /// bookend. Called from `Session::new` AFTER the PTY spawn succeeds
    /// (`pid` captured at spawn, uncontended).
    ///
    /// Failure here must not fail session creation — the caller maps Err to
    /// "session runs unlogged" (warn; ack1-spec §1 failure policy).
    ///
    /// `pid_start_ms`/`boot_id` are the agent-pid identity token (W2 / SPEC-v2
    /// §5.A) of `pid` — captured by the caller from the SAME `pid` recorded
    /// here (never `std::process::id()`, the daemon). Both `None` is fail-safe
    /// (the field is omitted; bond treats absence as crash-dead).
    pub fn open(
        ctx: &EventsCtx,
        session: &str,
        pid: u32,
        pid_start_ms: Option<u64>,
        boot_id: Option<String>,
    ) -> anyhow::Result<Self> {
        create_events_dir(&ctx.dir)?;
        // Epoch derivation: exact-structure scan (never a glob), then
        // O_CREAT|O_EXCL with bounded EEXIST retry (belt: per-session single
        // ownership should make collisions impossible).
        let start = scan_max_epoch(&ctx.dir, session).saturating_add(1);
        let (file, epoch) = open_with_retry(&ctx.dir, session, start)?;
        let writer = Self {
            session: session.to_string(),
            epoch,
            closed: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                file,
                seq: 0,
                bytes_written: 0,
                truncated: false,
                max_bytes: ctx.max_bytes,
            }),
        };
        let schema_version = DAEMON_EVENTS_SCHEMA_VERSION;
        writer.emit(
            move |meta| DaemonEvent::SessionOpened {
                meta,
                pid,
                schema_version,
                pid_start_ms,
                boot_id,
            },
            /* terminal */ false,
            /* is_close */ false,
        );
        Ok(writer)
    }

    /// `pty-bytes-written` — call ONLY while holding the pty_writer lock
    /// (seq order == byte order; ack1-spec §2 site W).
    pub fn emit_bytes_written(&self, bytes: u64, content_sha256: String, content_len: u64) {
        self.emit(
            move |meta| DaemonEvent::PtyBytesWritten {
                meta,
                bytes,
                content_sha256,
                content_len,
            },
            false,
            false,
        );
    }

    /// `pty-write-failed` — same locking rule as [`Self::emit_bytes_written`].
    pub fn emit_write_failed(
        &self,
        errno: Option<i32>,
        error: String,
        content_sha256: String,
        content_len: u64,
    ) {
        self.emit(
            move |meta| DaemonEvent::PtyWriteFailed {
                meta,
                errno,
                error,
                content_sha256,
                content_len,
            },
            false,
            false,
        );
    }

    /// `session-closed` bookend — idempotent (shared once-flag): the first
    /// caller wins, later calls (incl. the Drop belt) are no-ops.
    pub fn emit_closed(&self, reason: CloseReason) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.emit(
            move |meta| DaemonEvent::SessionClosed { meta, reason },
            /* terminal */ true,
            /* is_close */ true,
        );
    }

    /// Optional heartbeat tick (SBMUX_EVENTS_HEARTBEAT_MS). No-op after close
    /// (benign race with session teardown).
    pub fn emit_heartbeat(&self) {
        self.emit(|meta| DaemonEvent::Heartbeat { meta }, false, false);
    }

    /// True once the close bookend was emitted (heartbeat task uses this).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Single emission path: seq/cap/append under one lock (ack1-spec §1
    /// rotation accounting). `terminal` = terminal-class (admitted within the
    /// post-cap reserve). Emission failure never propagates (advisory stream;
    /// warn once per writer).
    fn emit<F>(&self, build: F, terminal: bool, is_close: bool)
    where
        F: FnOnce(EventMeta) -> DaemonEvent,
    {
        if !is_close && self.closed.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut g) = self.inner.lock() else {
            self.warn_once("events mutex poisoned");
            return;
        };
        let g = &mut *g;

        // Boundary rule (ack1-spec §1, ordering pinned here): when a record
        // would cross the cap, the events-truncated MARKER takes the seq at
        // the boundary position (in-file seq stays ordered); the crossing
        // record itself is then admitted (terminal-class, within reserve) or
        // suppressed — suppressed records still consume seq, so readers see a
        // gap = positive drop signal.
        let emit_one = |g: &mut Inner, ev_line: String, seq_consuming_only: bool| {
            if seq_consuming_only {
                return;
            }
            let line_len = ev_line.len() as u64;
            if let Err(e) = g
                .file
                .write_all(ev_line.as_bytes())
                .and_then(|_| g.file.flush())
            {
                self.warn_once(&format!("events append failed: {e}"));
                return;
            }
            g.bytes_written += line_len;
        };

        // Serialize with the next seq.
        g.seq += 1;
        let meta = EventMeta {
            session: self.session.clone(),
            epoch: self.epoch,
            seq: g.seq,
            ts_ms: now_ms(),
        };
        let ev = build(meta);
        let line = match serde_json::to_string(&ev) {
            Ok(mut s) => {
                s.push('\n');
                s
            }
            Err(e) => {
                self.warn_once(&format!("events serialize failed: {e}"));
                return;
            }
        };

        if g.truncated {
            // Post-cap regime: terminal-class only, within the reserve.
            let admit = terminal
                && g.bytes_written + line.len() as u64 <= g.max_bytes + TERMINAL_RESERVE_BYTES;
            if !admit && terminal {
                // Second-order exhaustion (named in spec §1): even the
                // reserve is full — drop with a warn.
                self.warn_once("events terminal reserve exhausted; dropping terminal record");
            }
            emit_one(g, line, !admit);
            return;
        }

        if g.bytes_written + line.len() as u64 > g.max_bytes {
            // Crossing record: the marker takes THIS seq position; re-serialize
            // the record with the NEXT seq and admit/suppress by class.
            g.truncated = true;
            let marker_seq = match &ev {
                DaemonEvent::SessionOpened { meta, .. }
                | DaemonEvent::PtyBytesWritten { meta, .. }
                | DaemonEvent::PtyWriteFailed { meta, .. }
                | DaemonEvent::SessionClosed { meta, .. }
                | DaemonEvent::EventsTruncated { meta, .. }
                | DaemonEvent::Heartbeat { meta } => meta.seq,
            };
            let marker = DaemonEvent::EventsTruncated {
                meta: EventMeta {
                    session: self.session.clone(),
                    epoch: self.epoch,
                    seq: marker_seq,
                    ts_ms: now_ms(),
                },
                cap_bytes: g.max_bytes,
            };
            if let Ok(mut mline) = serde_json::to_string(&marker) {
                mline.push('\n');
                let within_reserve =
                    g.bytes_written + mline.len() as u64 <= g.max_bytes + TERMINAL_RESERVE_BYTES;
                emit_one(g, mline, !within_reserve);
            }
            // The crossing record consumes the next seq (gap if suppressed).
            g.seq += 1;
            let mut rebuilt = ev;
            set_seq(&mut rebuilt, g.seq);
            let admit = terminal;
            if admit {
                if let Ok(mut rline) = serde_json::to_string(&rebuilt) {
                    rline.push('\n');
                    let within_reserve = g.bytes_written + rline.len() as u64
                        <= g.max_bytes + TERMINAL_RESERVE_BYTES;
                    emit_one(g, rline, !within_reserve);
                }
            }
            return;
        }

        emit_one(g, line, false);
    }

    fn warn_once(&self, msg: &str) {
        if !self.warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                session = %self.session,
                epoch = self.epoch,
                "{msg} — further event-emission warnings for this file suppressed"
            );
        }
    }
}

/// Mutate the seq inside an already-built event (cap-boundary re-serialize).
fn set_seq(ev: &mut DaemonEvent, seq: u64) {
    match ev {
        DaemonEvent::SessionOpened { meta, .. }
        | DaemonEvent::PtyBytesWritten { meta, .. }
        | DaemonEvent::PtyWriteFailed { meta, .. }
        | DaemonEvent::SessionClosed { meta, .. }
        | DaemonEvent::EventsTruncated { meta, .. }
        | DaemonEvent::Heartbeat { meta } => meta.seq = seq,
    }
}

/// Wall clock in Unix epoch milliseconds (0 on clock error — forensic field,
/// never load-bearing).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Create the events dir with 0o700 + the lstat symlink-refusal belt (same
/// posture as the socket dir, socket.rs).
fn create_events_dir(dir: &Path) -> anyhow::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(dir)
                .map_err(|e| anyhow::anyhow!("cannot stat events directory {dir:?}: {e}"))?;
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "events directory {dir:?} is a symlink — possible symlink attack, refusing"
                );
            }
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(
            "failed to create events directory {dir:?}: {e}"
        )),
    }
}

/// O_CREAT|O_EXCL open at `start_epoch`, walking forward on EEXIST (bounded,
/// 8 tries). Factored out of [`EventWriter::open`] so the retry belt is
/// unit-testable (R-EPOCH).
fn open_with_retry(dir: &Path, session: &str, start_epoch: u64) -> anyhow::Result<(File, u64)> {
    let mut epoch = start_epoch;
    for _ in 0..8 {
        let path = dir.join(format!("{session}.daemon.{epoch}.jsonl"));
        match std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((f, epoch)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                epoch = epoch.saturating_add(1);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("cannot create events file {path:?}: {e}"));
            }
        }
    }
    anyhow::bail!("events epoch EEXIST retry exhausted for session '{session}' in {dir:?}")
}

/// Scan for the max existing epoch of `session` in `dir`.
///
/// EXACT-STRUCTURE parse, never a glob (ack1-spec §1, red-team F4): session
/// names may legally contain `.` and even the substring `.daemon.`
/// (validate_session_name allows `[a-zA-Z0-9_-.]`). The all-digits anchor
/// makes cross-session contamination impossible: for
/// `X + ".daemon." + n == Y + ".daemon." + m` with n,m digit-only, the LAST
/// `.daemon.` occurrence in each side pins n == m and X == Y — two distinct
/// sessions cannot produce or match each other's filenames.
fn scan_max_epoch(dir: &Path, session: &str) -> u64 {
    let mut max = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(session) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(".daemon.") else {
            continue;
        };
        let Some(digits) = rest.strip_suffix(".jsonl") else {
            continue;
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = digits.parse::<u64>() {
            max = max.max(n);
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(seq: u64) -> EventMeta {
        EventMeta {
            session: "g1".into(),
            epoch: 1,
            seq,
            ts_ms: 1780722000123,
        }
    }

    /// R-SER: byte-exact golden lines for every variant (ack1-spec §3 examples;
    /// both errno branches). Field order is pinned by declaration order +
    /// serde_json determinism (Cargo.lock-pinned).
    #[test]
    fn r_ser_golden_lines_byte_exact() {
        let sha = "a".repeat(64);
        let cases: Vec<(DaemonEvent, String)> = vec![
            (
                // v2 token-PRESENT: the pinned wire-format example (§W1.1 /
                // EVENT-CONTRACT.md). Field order = declaration order: flattened
                // EventMeta, then pid, schema_version, then the appended token.
                DaemonEvent::SessionOpened {
                    meta: meta(1),
                    pid: 4242,
                    schema_version: 2,
                    pid_start_ms: Some(1780721999123),
                    boot_id: Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
                },
                r#"{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":2,"pid_start_ms":1780721999123,"boot_id":"550e8400-e29b-41d4-a716-446655440000"}"#.to_string(),
            ),
            (
                // v2 token-ABSENT: both token fields None → omit-when-None keeps
                // the line shape byte-identical to v1 (only schema_version bumps).
                DaemonEvent::SessionOpened {
                    meta: meta(1),
                    pid: 4242,
                    schema_version: 2,
                    pid_start_ms: None,
                    boot_id: None,
                },
                r#"{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":2}"#.to_string(),
            ),
            (
                DaemonEvent::PtyBytesWritten {
                    meta: meta(2),
                    bytes: 21,
                    content_sha256: sha.clone(),
                    content_len: 21,
                },
                format!(
                    r#"{{"event":"pty-bytes-written","session":"g1","epoch":1,"seq":2,"ts_ms":1780722000123,"bytes":21,"content_sha256":"{sha}","content_len":21}}"#
                ),
            ),
            (
                DaemonEvent::PtyWriteFailed {
                    meta: meta(3),
                    errno: Some(5),
                    error: "Input/output error (os error 5)".into(),
                    content_sha256: sha.clone(),
                    content_len: 21,
                },
                format!(
                    r#"{{"event":"pty-write-failed","session":"g1","epoch":1,"seq":3,"ts_ms":1780722000123,"errno":5,"error":"Input/output error (os error 5)","content_sha256":"{sha}","content_len":21}}"#
                ),
            ),
            (
                DaemonEvent::PtyWriteFailed {
                    meta: meta(4),
                    errno: None,
                    error: "pty_writer mutex poisoned".into(),
                    content_sha256: sha.clone(),
                    content_len: 21,
                },
                format!(
                    r#"{{"event":"pty-write-failed","session":"g1","epoch":1,"seq":4,"ts_ms":1780722000123,"errno":null,"error":"pty_writer mutex poisoned","content_sha256":"{sha}","content_len":21}}"#
                ),
            ),
            (
                DaemonEvent::SessionClosed {
                    meta: meta(5),
                    reason: CloseReason::Killed,
                },
                r#"{"event":"session-closed","session":"g1","epoch":1,"seq":5,"ts_ms":1780722000123,"reason":"killed"}"#.to_string(),
            ),
            (
                DaemonEvent::EventsTruncated {
                    meta: meta(6),
                    cap_bytes: 4096,
                },
                r#"{"event":"events-truncated","session":"g1","epoch":1,"seq":6,"ts_ms":1780722000123,"cap_bytes":4096}"#.to_string(),
            ),
            (
                DaemonEvent::Heartbeat { meta: meta(7) },
                r#"{"event":"heartbeat","session":"g1","epoch":1,"seq":7,"ts_ms":1780722000123}"#.to_string(),
            ),
        ];
        for (ev, golden) in cases {
            let got = serde_json::to_string(&ev).unwrap();
            assert_eq!(got, golden, "golden mismatch for {ev:?}");
            // Round-trip.
            let back: DaemonEvent = serde_json::from_str(&got).unwrap();
            assert_eq!(back, ev);
        }
        // All close reasons serialize kebab-case.
        for (r, s) in [
            (CloseReason::Killed, "killed"),
            (CloseReason::ChildExited, "child-exited"),
            (CloseReason::DaemonShutdown, "daemon-shutdown"),
            (CloseReason::Dropped, "dropped"),
        ] {
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{s}\""));
        }
    }

    /// R-SER forward-compat: unknown FIELDS tolerated on typed parse (rule (a)/(e));
    /// unknown EVENT variants skipped via parse_line, never a panic (rule (b)).
    #[test]
    fn r_ser_forward_compat_tolerance() {
        // Unknown extra field on a known variant → parses.
        let line = r#"{"event":"session-closed","session":"g1","epoch":1,"seq":5,"ts_ms":1,"reason":"killed","future_field":"x"}"#;
        let ev = parse_line(line).expect("unknown field must be tolerated");
        assert!(matches!(ev, DaemonEvent::SessionClosed { .. }));
        // Unknown event variant → None, no panic.
        let line = r#"{"event":"future-thing","session":"g1","epoch":1,"seq":9,"ts_ms":1}"#;
        assert!(parse_line(line).is_none());
        // Malformed line → None, no panic.
        assert!(parse_line("{torn").is_none());
    }

    /// W2 §5.1 — the BIDIRECTIONAL back-compat fold (the headline guard).
    #[test]
    fn r_token_back_compat_fold_bidirectional() {
        // (a) A PRE-TOKEN (v1) `session-opened` — no token fields — parses under
        //     the NEW (v2) reader; both fields default to None (additive rule
        //     (a), `#[serde(default)]`), no error.
        let pre = r#"{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":1}"#;
        match parse_line(pre).expect("pre-token line must parse under the v2 reader") {
            DaemonEvent::SessionOpened {
                pid,
                schema_version,
                pid_start_ms,
                boot_id,
                ..
            } => {
                assert_eq!(pid, 4242);
                assert_eq!(schema_version, 1);
                assert_eq!(pid_start_ms, None, "absent token field defaults to None");
                assert_eq!(boot_id, None);
            }
            other => panic!("expected session-opened, got {other:?}"),
        }

        // (b) A POST-TOKEN (v2) line parses under a FAITHFUL pre-W2 reader (F3).
        //     This mirrors the REAL pre-W2 `SessionOpened` shape — `#[serde(flatten)]
        //     meta: EventMeta` + `pid` + `schema_version`, no token fields, no
        //     `deny_unknown_fields` (rule (e)) — NOT a direct-field proxy. It thus
        //     exercises the subtle serde `flatten` + internal-tag + unknown-field
        //     routing corner: the extra `pid_start_ms`/`boot_id` fall through to the
        //     flattened `EventMeta` (which ignores unknowns) → parsed, no error.
        #[derive(serde::Deserialize)]
        #[serde(tag = "event", rename_all = "kebab-case")]
        enum OldDaemonEvent {
            SessionOpened {
                #[serde(flatten)]
                meta: EventMeta,
                pid: u32,
                schema_version: u32,
            },
        }
        let post = r#"{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":2,"pid_start_ms":1780721999123,"boot_id":"550e8400-e29b-41d4-a716-446655440000"}"#;
        match serde_json::from_str::<OldDaemonEvent>(post).expect(
            "a v2 token line must parse under the faithful pre-W2 (flatten) reader — no deny_unknown_fields",
        ) {
            OldDaemonEvent::SessionOpened {
                meta,
                pid,
                schema_version,
            } => {
                // The flatten target captured the envelope; the unknown token
                // fields were ignored (not misrouted, not errored).
                assert_eq!(meta.session, "g1");
                assert_eq!(meta.epoch, 1);
                assert_eq!(meta.seq, 1);
                assert_eq!(meta.ts_ms, 1780722000123);
                assert_eq!(pid, 4242);
                assert_eq!(schema_version, 2);
            }
        }
    }

    /// F1 — pin the ACTUAL `pid==0` emission shape: a `boot_id`-only half-token.
    /// On `pid==0`, `pid_start_ms(0)` is `None` (omitted) but `boot_id()` is
    /// pid-INDEPENDENT and may be present → the emitted `session-opened` carries
    /// `pid:0`, `boot_id` present, `pid_start_ms` OMITTED. `boot_id` is NOT gated to
    /// `None` on `pid==0` (that would be a needless special case); the line is still
    /// fail-safe because a consumer treats absent `pid_start_ms` as crash-dead
    /// regardless of `boot_id`. Locks code ↔ contract (matches EVENT-CONTRACT.md §2.3).
    #[test]
    fn r_session_opened_pid0_half_token_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = EventsCtx {
            dir: tmp.path().join("events"),
            max_bytes: DEFAULT_MAX_BYTES,
        };
        create_events_dir(&ctx.dir).unwrap();
        // Models the child_pid==0 chain at the emission boundary: session.rs passes
        // pid_start_ms(0)=None and boot_id()=Some(..) into open.
        let _w = EventWriter::open(
            &ctx,
            "s",
            0,
            None,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
        )
        .unwrap();
        let content = std::fs::read_to_string(ctx.dir.join("s.daemon.1.jsonl")).unwrap();
        let line = content.lines().next().unwrap();
        assert!(line.contains(r#""pid":0"#), "pid:0 recorded: {line}");
        assert!(
            line.contains(r#""boot_id":"550e8400-e29b-41d4-a716-446655440000""#),
            "boot_id present on pid==0 (pid-independent): {line}"
        );
        assert!(
            !line.contains("pid_start_ms"),
            "pid_start_ms OMITTED when None (the half-token shape): {line}"
        );
        match parse_line(line).unwrap() {
            DaemonEvent::SessionOpened {
                pid,
                pid_start_ms,
                boot_id,
                ..
            } => {
                assert_eq!(pid, 0);
                assert_eq!(pid_start_ms, None);
                assert_eq!(
                    boot_id.as_deref(),
                    Some("550e8400-e29b-41d4-a716-446655440000")
                );
            }
            other => panic!("expected session-opened, got {other:?}"),
        }
    }

    /// F2 — forward-compat: a FUTURE `schema_version:3` `session-opened` carrying an
    /// unknown future field parses under the v2 reader, the unknown field ignored,
    /// no error (the additive-evolution "never fail on a newer-than-known version"
    /// guarantee; `deny_unknown_fields` banned, rule (e)). The fold/corpus tests
    /// otherwise stop at v2 — this closes that coverage gap.
    #[test]
    fn r_token_forward_compat_v3_line_parses() {
        let v3 = r#"{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":3,"pid_start_ms":1780721999123,"boot_id":"550e8400-e29b-41d4-a716-446655440000","future_token_field":"ignored"}"#;
        match parse_line(v3)
            .expect("a future v3 + unknown-field line must parse under the v2 reader, not error")
        {
            DaemonEvent::SessionOpened {
                pid,
                schema_version,
                pid_start_ms,
                boot_id,
                ..
            } => {
                assert_eq!(schema_version, 3, "the newer version is READ, not rejected");
                assert_eq!(pid, 4242);
                assert_eq!(
                    pid_start_ms,
                    Some(1780721999123),
                    "known token field still read"
                );
                assert_eq!(
                    boot_id.as_deref(),
                    Some("550e8400-e29b-41d4-a716-446655440000")
                );
            }
            other => panic!("expected session-opened, got {other:?}"),
        }
    }

    /// W2.3 (N6c) — `open` stamps the token fields it is GIVEN onto the emitted
    /// `session-opened`, faithfully (pid == arg, token == arg). The value-pass-
    /// through that the same-pid invariant rests on (the caller wires `child_pid`
    /// + its token into these args).
    #[test]
    fn r_open_stamps_token_fields_from_args() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = EventsCtx {
            dir: tmp.path().join("events"),
            max_bytes: DEFAULT_MAX_BYTES,
        };
        create_events_dir(&ctx.dir).unwrap();
        let _w = EventWriter::open(
            &ctx,
            "s",
            4242,
            Some(1780721999123),
            Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
        )
        .unwrap();
        let content = std::fs::read_to_string(ctx.dir.join("s.daemon.1.jsonl")).unwrap();
        match parse_line(content.lines().next().unwrap()).unwrap() {
            DaemonEvent::SessionOpened {
                pid,
                schema_version,
                pid_start_ms,
                boot_id,
                ..
            } => {
                assert_eq!(pid, 4242, "emitted pid == the pid arg (the recorded child)");
                assert_eq!(schema_version, 2, "v2 stamp");
                assert_eq!(
                    pid_start_ms,
                    Some(1780721999123),
                    "token start-time passed through"
                );
                assert_eq!(
                    boot_id.as_deref(),
                    Some("550e8400-e29b-41d4-a716-446655440000"),
                    "token boot_id passed through"
                );
            }
            other => panic!("expected session-opened, got {other:?}"),
        }
    }

    /// W1 §5.7 — contract-stability regression guard: a frozen corpus of real
    /// mux lines (pre-token v1, v2 token-present, v2 token-absent, plus other
    /// families) all fold via the documented reader (`parse_line`) with no
    /// error; a torn trailing line is skipped (`parse_line → None`). Proves the
    /// W2 change PRESERVED the readable contract. (The delivery family's
    /// stability is the `dispatch` crate's own reader tests; the cross-family
    /// path-prefix discriminator is documented in doc/EVENT-CONTRACT.md.)
    #[test]
    fn r_contract_stability_mux_corpus_folds() {
        let corpus = [
            r#"{"event":"session-opened","session":"g1","epoch":1,"seq":1,"ts_ms":1780722000123,"pid":4242,"schema_version":1}"#,
            r#"{"event":"session-opened","session":"g1","epoch":2,"seq":1,"ts_ms":1780722000999,"pid":4243,"schema_version":2,"pid_start_ms":1780721999123,"boot_id":"550e8400-e29b-41d4-a716-446655440000"}"#,
            r#"{"event":"session-opened","session":"g1","epoch":3,"seq":1,"ts_ms":1780722001000,"pid":4244,"schema_version":2}"#,
            r#"{"event":"pty-bytes-written","session":"g1","epoch":1,"seq":2,"ts_ms":1780722000200,"bytes":21,"content_sha256":"aaaa","content_len":21}"#,
            r#"{"event":"session-closed","session":"g1","epoch":1,"seq":3,"ts_ms":1780722000300,"reason":"daemon-shutdown"}"#,
        ];
        for line in corpus {
            assert!(parse_line(line).is_some(), "corpus line must fold: {line}");
        }
        // Torn trailing line (an interrupted ≤cap append) → skipped, never error.
        assert!(parse_line(r#"{"event":"session-opened","session":"g1","epo"#).is_none());
    }

    /// R-EPOCH: derivation incl. the adversarial prefix-collision case
    /// (sessions `g1` and `g1.daemon.9` coexisting — red-team F4).
    #[test]
    fn r_epoch_exact_structure_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Empty dir → max 0 → first epoch 1.
        assert_eq!(scan_max_epoch(dir, "g1"), 0);

        let touch = |name: &str| std::fs::write(dir.join(name), b"").unwrap();
        touch("g1.daemon.1.jsonl");
        touch("g1.daemon.3.jsonl"); // gap is fine
        touch("g1.daemon.03.jsonl"); // leading zero: still digits, parses 3
        touch("g1.daemon.x.jsonl"); // garbage epoch → ignored
        touch("g1.daemon..jsonl"); // empty epoch → ignored
        touch("g1.daemon.99999999999999999999999.jsonl"); // overflow → ignored
        touch("g1.jsonl"); // wrong structure → ignored
        touch("other.daemon.7.jsonl"); // other session → ignored
                                       // The adversarial coexisting session: its files must NOT count for g1.
        touch("g1.daemon.9.daemon.2.jsonl"); // session "g1.daemon.9", epoch 2
        assert_eq!(scan_max_epoch(dir, "g1"), 3, "g1 sees only its own lineage");
        // ...and the dotted session sees only ITS lineage.
        assert_eq!(scan_max_epoch(dir, "g1.daemon.9"), 2);
        // REVERSE direction (merge-ruling C-4): g1's own epoch-9 file is
        // walked by g1 but must NOT be claimed by session "g1.daemon.9"
        // (stripping its full name leaves ".jsonl" — no ".daemon." follows).
        touch("g1.daemon.9.jsonl"); // session "g1", epoch 9
        assert_eq!(scan_max_epoch(dir, "g1"), 9, "g1 claims its own epoch 9");
        assert_eq!(
            scan_max_epoch(dir, "g1.daemon.9"),
            2,
            "the dotted session must not claim g1's epoch-9 file"
        );
        // A session that is a strict prefix of another's name.
        assert_eq!(
            scan_max_epoch(dir, "g1.daemon"),
            0,
            "no .daemon. follows the literal"
        );
    }

    /// R-EPOCH: O_EXCL EEXIST retry walks forward (belt exercised directly);
    /// exhaustion errors, no panic.
    #[test]
    fn r_epoch_excl_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("events");
        create_events_dir(&dir).unwrap();
        // The retry belt itself: epochs 1 and 2 pre-planted, start at 1 → 3.
        std::fs::write(dir.join("s.daemon.1.jsonl"), b"").unwrap();
        std::fs::write(dir.join("s.daemon.2.jsonl"), b"").unwrap();
        let (_f, epoch) = open_with_retry(&dir, "s", 1).unwrap();
        assert_eq!(epoch, 3, "EEXIST walks forward");
        std::fs::remove_file(dir.join("s.daemon.3.jsonl")).unwrap();
        // Exhaustion: plant 8 consecutive epochs from the start point.
        for e in 10..18 {
            std::fs::write(dir.join(format!("x.daemon.{e}.jsonl")), b"").unwrap();
        }
        let err = open_with_retry(&dir, "x", 10).unwrap_err().to_string();
        assert!(err.contains("retry exhausted"), "got: {err}");

        // Scan + open compose end-to-end: scan sees 1,2 → opens 3 with the
        // session-opened bookend.
        let ctx = EventsCtx {
            dir: dir.clone(),
            max_bytes: DEFAULT_MAX_BYTES,
        };
        let w = EventWriter::open(&ctx, "s", 1, None, None).unwrap();
        assert_eq!(w.epoch, 3);
        let content = std::fs::read_to_string(dir.join("s.daemon.3.jsonl")).unwrap();
        let first = parse_line(content.lines().next().unwrap()).unwrap();
        match first {
            DaemonEvent::SessionOpened {
                meta,
                pid,
                schema_version,
                ..
            } => {
                assert_eq!(meta.seq, 1);
                assert_eq!(meta.epoch, 3);
                assert_eq!(pid, 1);
                assert_eq!(schema_version, DAEMON_EVENTS_SCHEMA_VERSION);
            }
            other => panic!("first record must be session-opened, got {other:?}"),
        }
    }

    /// R-SEQ: monotonic, session-opened == 1, suppressed records consume seq
    /// (tiny cap forces the truncated regime).
    #[test]
    fn r_seq_monotonic_and_suppression_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = EventsCtx {
            dir: tmp.path().join("events"),
            max_bytes: 400,
        };
        create_events_dir(&ctx.dir).unwrap();
        let w = EventWriter::open(&ctx, "s", 7, None, None).unwrap();
        let sha = "b".repeat(64);
        // Each pty-bytes-written line is ~170 bytes; the cap (400) admits the
        // bookend + one write, then the second write crosses → marker + gap.
        for _ in 0..4 {
            w.emit_bytes_written(8, sha.clone(), 8);
        }
        w.emit_closed(CloseReason::Killed);
        let content = std::fs::read_to_string(ctx.dir.join("s.daemon.1.jsonl")).unwrap();
        let evs: Vec<DaemonEvent> = content.lines().filter_map(parse_line).collect();
        // Structure: opened(1), some written, exactly one truncated marker,
        // then ONLY terminal-class, ending in closed.
        assert!(
            matches!(evs.first(), Some(DaemonEvent::SessionOpened { meta, .. }) if meta.seq == 1)
        );
        let truncated_count = evs
            .iter()
            .filter(|e| matches!(e, DaemonEvent::EventsTruncated { .. }))
            .count();
        assert_eq!(truncated_count, 1, "exactly one events-truncated marker");
        let closed = evs.iter().any(|e| {
            matches!(
                e,
                DaemonEvent::SessionClosed {
                    reason: CloseReason::Killed,
                    ..
                }
            )
        });
        assert!(closed, "close bookend admitted in the terminal reserve");
        // Seq strictly increasing in-file; a GAP exists after the marker
        // (suppressed writes consumed seq).
        let seqs: Vec<u64> = evs
            .iter()
            .map(|e| match e {
                DaemonEvent::SessionOpened { meta, .. }
                | DaemonEvent::PtyBytesWritten { meta, .. }
                | DaemonEvent::PtyWriteFailed { meta, .. }
                | DaemonEvent::SessionClosed { meta, .. }
                | DaemonEvent::EventsTruncated { meta, .. }
                | DaemonEvent::Heartbeat { meta } => meta.seq,
            })
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "in-file seq strictly increasing: {seqs:?}"
        );
        let max_seq = *seqs.last().unwrap();
        assert!(
            max_seq as usize > evs.len(),
            "suppressed records left a seq gap: max {max_seq} > {} records",
            evs.len()
        );
        // No non-terminal record after the marker.
        let marker_idx = evs
            .iter()
            .position(|e| matches!(e, DaemonEvent::EventsTruncated { .. }))
            .unwrap();
        assert!(
            evs[marker_idx + 1..].iter().all(|e| matches!(
                e,
                DaemonEvent::SessionClosed { .. } | DaemonEvent::EventsTruncated { .. }
            )),
            "only terminal-class after the marker"
        );
    }

    /// Close once-flag: second close (and the Drop belt) is a no-op.
    #[test]
    fn close_bookend_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = EventsCtx {
            dir: tmp.path().join("events"),
            max_bytes: DEFAULT_MAX_BYTES,
        };
        create_events_dir(&ctx.dir).unwrap();
        let w = EventWriter::open(&ctx, "s", 7, None, None).unwrap();
        w.emit_closed(CloseReason::Killed);
        w.emit_closed(CloseReason::Dropped); // belt path after explicit close
        w.emit_heartbeat(); // post-close emit is a no-op
        let content = std::fs::read_to_string(ctx.dir.join("s.daemon.1.jsonl")).unwrap();
        let closes = content
            .lines()
            .filter_map(parse_line)
            .filter(|e| matches!(e, DaemonEvent::SessionClosed { .. }))
            .count();
        assert_eq!(closes, 1, "exactly one close bookend");
        assert!(!content.contains("heartbeat"), "no post-close records");
    }

    /// R-DIR: events-dir symlink refusal (same belt posture as the socket dir).
    #[test]
    fn r_dir_symlink_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("events");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = create_events_dir(&link).unwrap_err().to_string();
        assert!(err.contains("symlink"), "got: {err}");
    }
}
