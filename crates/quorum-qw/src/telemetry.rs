//! A6 telemetry — FRESH design (shape ruled ADD-3a/3b; spec §4). The single
//! `marks.jsonl` stream gains TWO new line kinds and a pure snapshot fold.
//!
//! # Module name
//!
//! `telemetry` (not `lineage`): it owns BOTH the lineage `create` events AND the
//! content-free `invoked` occurrence records AND the snapshot fold — broader than
//! lineage alone. The spec offered either name; this is the wider hat.
//!
//! # Line kinds in `marks.jsonl` (single stream, one file, O_APPEND 0600)
//!
//! - **Mark lines (existing, UNTOUCHED):** `{"ts","sessionId","payload"}` — the
//!   engine NEVER inspects `payload` keys (ADD-3a/3b). Built by `mark.rs`.
//! - **Engine event lines (new):** distinguished by a top-level `"event"` key.
//!   Mark lines have `"payload"` and never `"event"`; event lines never carry
//!   `"payload"`. A `"event"` key INSIDE a mark payload stays inside the payload
//!   object and is NEVER confused with a top-level event line (the fold keys on
//!   the TOP-LEVEL `"event"` only). No collision.
//!   - create: `{"ts","event":"create","name",sessionId?,spawnedBy?,
//!     spawnedBySessionId?,backend?}`
//!   - invoked:  `{"ts","event":"invoked","verb",sessionId?,name?}`
//!
//! # Engine-content-free discipline (Pete-ruled ADD-3a/3b)
//!
//! The engine never inspects mark payload KEYS. No org vocabulary
//! (on_behalf_of/role_claimed/reports_to/succeeds) anywhere here. The event lines
//! carry ONLY the engine-owned fields above (lineage NAMES + the via-backend
//! name + a verb tag) — never message bodies, payloads, or byte counts.
//!
//! # Durability posture (spec §4.1, red-team F8)
//!
//! Event/invoked lines are small (sub-`PIPE_BUF`) → an `O_APPEND` single write is
//! atomic for THEM. `qd mark` payloads above `PIPE_BUF` can interleave under
//! concurrency — a documented v1 limitation (fresh surface, no TS counterpart).
//! Append failures on invoked/create lines are NON-FATAL (the caller warns to
//! stderr and leaves the verb's exit code unchanged) — telemetry must never break
//! a working verb.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::effects::{Clock, Env};
use crate::exec::Exec;
use crate::paths::QdPaths;
use crate::registry::{self, RegistryEntry};
use quorum_core::timefmt::epoch_ms_to_iso;

// ===========================================================================
// Caller-identity ppid walk (hoisted from verbs/whoami.rs — spec §4.2 / F11)
// ===========================================================================

/// Port of `findCallerSession` (commands/status.ts:159-184). Start at the
/// caller's parent PID, walk UP the process tree up to 10 levels; at each PID try
/// to read `<sessions_dir>/<pid>.json` (the registry entry) — first hit wins.
/// Walk up via `ps -o ppid= -p <pid>` through the injected [`Exec`] seam.
///
/// Behavior-preserving HOIST of `whoami.rs`'s private fn (F11): identical walk,
/// identical stop conditions (`pid <= 1`, unreadable parent). The Exec seam is
/// now a parameter (was a hard-coded `RealExec`) so create.rs and unit tests can
/// inject a fake — a pure signature generalization, no behavior change for the
/// real-deps caller (`whoami` passes `&RealExec`).
pub fn find_caller_session(paths: &QdPaths, exec: &dyn Exec) -> Option<RegistryEntry> {
    let mut pid: i32 = unsafe { libc::getppid() };
    for _ in 0..10 {
        if pid <= 1 {
            break;
        }
        if let Some(entry) = registry::read_entry(&paths.sessions_dir, pid as i64) {
            return Some(entry);
        }
        // Step to the parent: `ps -o ppid= -p <pid>` (commands/status.ts:170-178).
        let parent = ps_ppid(exec, pid)?;
        if parent <= 1 {
            break;
        }
        pid = parent;
    }
    None
}

/// `ps -o ppid= -p <pid>` → the parent pid, or `None` on any error / non-numeric
/// output (TS `catch { break }` / `isNaN(parent)`).
///
/// Generalized from `&RealExec` to `&dyn Exec` (F11) — the ONLY behavior change
/// is that a substitutable Exec is now accepted; the real path is identical.
pub fn ps_ppid(exec: &dyn Exec, pid: i32) -> Option<i32> {
    let args = vec![
        "-o".to_string(),
        "ppid=".to_string(),
        "-p".to_string(),
        pid.to_string(),
    ];
    let out = exec.run("ps", &args, &[], None, None).ok()?;
    out.stdout.trim().parse::<i32>().ok()
}

// ===========================================================================
// Marks path (shared with mark.rs's resolution — QD_HOME-honoring, L9a)
// ===========================================================================

/// Resolve `<qdHome>/state/marks.jsonl` via `QdPaths::from_home_env` (honors
/// QD_HOME through the injected `Env` seam; L9a). `None` if HOME is unset. Same
/// resolution `mark.rs` uses, hoisted so create/send/wait/mark all agree.
pub fn marks_path(env: &dyn Env) -> Option<PathBuf> {
    let home = env.var("HOME").filter(|s| !s.is_empty())?;
    let paths = QdPaths::from_home_env(Path::new(&home), env);
    Some(paths.state_dir.join("marks.jsonl"))
}

// ===========================================================================
// Event line builders (spec §4.1 — exact key order)
// ===========================================================================

/// A `create` lineage event (spec §4.1). `name` is the new session's name (the
/// only REQUIRED field beyond ts/event); the rest are present only when known.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreateEvent {
    pub name: String,
    pub session_id: Option<String>,
    pub spawned_by: Option<String>,
    pub spawned_by_session_id: Option<String>,
    pub backend: Option<String>,
}

/// Build the `create` event line — key order EXACTLY:
/// `ts, event, name, sessionId?, spawnedBy?, spawnedBySessionId?, backend?`
/// (serde_json `preserve_order`). Optional fields are OMITTED when `None` (never
/// emitted as empty strings — spec §4.2: "fields absent, never empty strings").
pub fn build_create_line(ts: &str, ev: &CreateEvent) -> String {
    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));
    obj.insert("event".into(), Value::String("create".into()));
    obj.insert("name".into(), Value::String(ev.name.clone()));
    insert_opt(&mut obj, "sessionId", &ev.session_id);
    insert_opt(&mut obj, "spawnedBy", &ev.spawned_by);
    insert_opt(&mut obj, "spawnedBySessionId", &ev.spawned_by_session_id);
    insert_opt(&mut obj, "backend", &ev.backend);
    Value::Object(obj).to_string()
}

/// Build an `invoked` line — key order EXACTLY: `ts, event, verb, sessionId?, name?`.
/// At least one of `session_id`/`name` is expected present (spec §4.1); the
/// builder does not enforce it (callers supply what they have — an invoked line
/// with neither is harmless to the fold, which simply can't key it).
pub fn build_invoked_line(
    ts: &str,
    verb: &str,
    session_id: Option<&str>,
    name: Option<&str>,
) -> String {
    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));
    obj.insert("event".into(), Value::String("invoked".into()));
    obj.insert("verb".into(), Value::String(verb.to_string()));
    if let Some(s) = session_id.filter(|s| !s.is_empty()) {
        obj.insert("sessionId".into(), Value::String(s.to_string()));
    }
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        obj.insert("name".into(), Value::String(n.to_string()));
    }
    Value::Object(obj).to_string()
}

fn insert_opt(obj: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(v) = v.as_ref().filter(|s| !s.is_empty()) {
        obj.insert(key.to_string(), Value::String(v.clone()));
    }
}

// ===========================================================================
// Appender (O_APPEND 0600 single write, create-parent — spec §4.1)
// ===========================================================================

/// Append one line + newline to `marks_path` via `O_APPEND` single write; create
/// the parent dir if missing. Mode 0600. Returns a human error on any failure.
///
/// Same discipline as `mark.rs`'s `append_mark` (independent fn, identical
/// behavior) — kept here so the event/invoked builders have a local appender that
/// the additive surfaces own without reaching into the verbs/ binary layer.
pub fn append_line(marks_path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = marks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create state dir: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(marks_path)
        .map_err(|e| format!("could not open {}: {e}", marks_path.display()))?;
    f.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("write failed: {e}"))
}

/// Append one self-DELIMITED `observed` record via a SINGLE atomic sub-`PIPE_BUF`
/// `O_APPEND` write of `\n{line}\n`. This is the observe-path appender; `append_line`
/// (bare `{line}\n`) stays for create/invoked.
///
/// # WRITER-torn safety (F-DEOBS-1, red-team round-1 finding)
///
/// The shared `marks.jsonl` stream can end in a TORN, non-newline-terminated tail —
/// a reader-TOLERATED state that a `>PIPE_BUF` `qd mark` payload can leave. Crucially
/// `qd mark`'s append does NOT take `observed.lock`, so the observe lock does not
/// serialize against it: a torn tail can appear between our locked scan and our
/// append. A bare `O_APPEND` of `{line}\n` would GLUE our record onto that torn
/// prefix, producing ONE unparseable line — the append is silently lost, yet the
/// marker still commits, breaking the load-bearing `marker present ⟹ readable line`
/// invariant (so the next call fast-paths to a false "already recorded" over an
/// effectively empty stream: a first sighting silently lost, violating clause (1)
/// in NORMAL operation).
///
/// The LEADING newline unconditionally CLOSES any torn prefix (leaving it as its own
/// line — still unparseable, still reader-skipped) and starts our complete record
/// fresh; the trailing newline terminates it. When the tail was already clean (or the
/// file empty), the leading newline merely yields a blank line, which every JSONL
/// reader skips (`fold_marks` and `observed_line_in_stream` both `continue` on an
/// empty trimmed line). Because the whole `\n{line}\n` is sub-`PIPE_BUF`, the
/// `O_APPEND` write is atomic and lands CONTIGUOUSLY even if an unrelated writer
/// interleaves — there is no peek-last-byte-then-append TOCTOU to lose against.
fn append_observed_self_delimited(marks_path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = marks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create state dir: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(marks_path)
        .map_err(|e| format!("could not open {}: {e}", marks_path.display()))?;
    // Single write of the whole `\n{line}\n` (sub-PIPE_BUF ⇒ one atomic O_APPEND).
    f.write_all(format!("\n{line}\n").as_bytes())
        .map_err(|e| format!("write failed: {e}"))
}

// ===========================================================================
// Public verb-facing API (the lead calls append_create_event from create.rs;
// send/wait/mark call append_invoked). NON-FATAL by contract — these return a
// Result the caller logs-and-ignores; they NEVER change a verb's exit code.
// ===========================================================================

/// Append a `create` lineage event line to `marks.jsonl`. Resolves the marks path
/// via the injected `Env` (QD_HOME-honoring). `Err` carries a human reason for the
/// caller to warn about; the caller MUST NOT change its exit code on failure
/// (spec §4.1 — telemetry is best-effort-durable).
pub fn append_create_event(
    env: &dyn Env,
    clock: &dyn Clock,
    ev: &CreateEvent,
) -> Result<(), String> {
    let path = marks_path(env)
        .ok_or_else(|| "HOME is not set — cannot resolve the state dir".to_string())?;
    let line = build_create_line(&epoch_ms_to_iso(clock.now_ms()), ev);
    append_line(&path, &line)
}

/// Append a content-free `invoked` line for a successful verb invocation. Same
/// best-effort contract as [`append_create_event`].
pub fn append_invoked(
    env: &dyn Env,
    clock: &dyn Clock,
    verb: &str,
    session_id: Option<&str>,
    name: Option<&str>,
) -> Result<(), String> {
    let path = marks_path(env)
        .ok_or_else(|| "HOME is not set — cannot resolve the state dir".to_string())?;
    let line = build_invoked_line(&epoch_ms_to_iso(clock.now_ms()), verb, session_id, name);
    append_line(&path, &line)
}

// ===========================================================================
// DE-observed — first-sighting-only idempotent recorder (spec S3.2)
//
// `record_observed` appends AT MOST ONE `observed` line per identity key
// `(host, harness, sessionId)` EVER, in one host's `marks.jsonl` stream, under
// concurrency. It records a discovered session as an IDENTITY FACT ONLY — no
// pid, no liveness, no status — and creates NO registry row (a discovered
// session never enters the pid-keyed live registry; this path never calls
// `registry::write_entry` / `set_status` / `claim_name`).
//
// # Line kind
//
// `{"ts","event":"observed","host","harness","sessionId","cwd"?}` — `ts` via
// `render::epoch_ms_to_iso` (the ONE timestamp format for this stream). `host`
// is EXPLICIT in the key by design: the same native session id on two different
// hosts is two legitimate facts (cross-host non-collision).
//
// # Mechanism — why it is correct under concurrency (spec S3.2 clauses 1 & 2)
//
// The load-bearing choice: **the `observed` stream line IS the claim.** There is
// no separate durable pre-append claim that a future call consults. The only
// durable state that gates a future write is the presence of the line itself.
// That structurally defeats the "O_EXCL claim marker" trap (claim wins → append
// fails/crashes → identity permanently unrecordable): here a failed or crashed
// append simply leaves no line, so the next call re-appends. Nothing can be
// claimed-but-never-recorded.
//
//   1. WRITE-TIME idempotency (NOT reader-side dedup): the check-then-append is
//      serialized by an exclusive `flock` on a dedicated `observed.lock`. The
//      losing racer BLOCKS on the lock; when it acquires, its scan of the stream
//      finds the winner's line and it writes NOTHING. In normal single-host
//      operation the stream never physically carries two `observed` lines with
//      the same key. (`flock` supplies the check-then-act atomicity that a bare
//      `O_APPEND` write cannot.)
//   2. Claim+append RECOVERABLE together: a failed append (or a crash) after
//      the check leaves NO line and NO marker → the next call re-appends. `flock`
//      is released by the OS when the holding fd closes, INCLUDING on process
//      death, so a crashed holder never wedges future observers. No path leaves
//      an identity permanently unrecordable.
//
// # WRITER-torn safety (F-DEOBS-1)
//
// The observe lock serializes observers against EACH OTHER, but the shared
// `marks.jsonl` also carries create/invoked/mark lines from writers that do NOT take
// `observed.lock` — and a `>PIPE_BUF` `qd mark` payload can leave a TORN,
// non-newline-terminated tail. So the `observed` append must be robust against a
// torn tail appearing between the locked scan and the write: it uses a SINGLE
// atomic sub-`PIPE_BUF` `O_APPEND` of `\n{line}\n` (see
// [`append_observed_self_delimited`]). The leading newline closes any torn prefix
// so our record is INDEPENDENTLY READABLE, keeping `marker present ⟹ readable
// line` intact even over a dirty tail (else a first sighting is silently lost while
// the marker commits — the clause-(1) violation this guard closes).
//
// # The per-key marker — a NON-AUTHORITATIVE fast path
//
// Correctness rests ENTIRELY on the lock + stream scan above. On top of that, a
// per-key marker file under `observed-claims/` is a pure performance hint: it is
// created ONLY AFTER a line commits, so `marker present ⟹ line committed`. The
// hot path (`marker present`) returns "already recorded" with a single `stat`,
// no lock, no scan — so a thousand `ls` runs append zero lines and, after the
// first sighting, do O(1) work. The marker can only ever cause an early NO-OP,
// never an early APPEND, so it cannot manufacture a duplicate. A lost/never-written
// marker is always safe: the locked stream scan is the fallback authority and
// self-heals the marker.
//
// # Retention boundary (spec QS-8)
//
// First-sighting-only is an integrity property of ONE stream's lifetime, not a
// global-history theorem. Stream loss/rotation ⇒ LEGITIMATE re-observation. The
// dedup memory lives in TWO coupled places — the stream lines AND the
// `observed-claims/` markers — so **rotating `marks.jsonl` MUST also clear
// `observed-claims/`**, else a surviving marker fast-paths to "already recorded"
// while the line is gone and re-observation silently can't happen.
//
// # Reader tolerance at composition seams (spec QS-6)
//
// This write-time property binds ONE host's stream in normal operation. Post-merge
// / post-loss history can legitimately hold duplicate keys; readers must treat
// them as ONE fact (first-wins). `fold_marks` already skips `observed` as an
// unknown (non-`create`) kind, so duplicates are inert to the fold regardless.
// ===========================================================================

/// Build the `observed` event line — key order EXACTLY:
/// `ts, event, host, harness, sessionId, cwd?` (serde_json `preserve_order`).
/// `cwd` is OMITTED when `None`/empty (never an empty string — spec §4.2 shape,
/// mirroring [`build_create_line`]/[`build_invoked_line`]). Identity-facts-only:
/// no pid, no liveness, no status.
pub fn build_observed_line(
    ts: &str,
    host: &str,
    harness: &str,
    session_id: &str,
    cwd: Option<&str>,
) -> String {
    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));
    obj.insert("event".into(), Value::String("observed".into()));
    obj.insert("host".into(), Value::String(host.to_string()));
    obj.insert("harness".into(), Value::String(harness.to_string()));
    obj.insert("sessionId".into(), Value::String(session_id.to_string()));
    if let Some(c) = cwd.filter(|c| !c.is_empty()) {
        obj.insert("cwd".into(), Value::String(c.to_string()));
    }
    Value::Object(obj).to_string()
}

/// Test seam for [`record_observed_in`] — injectable hooks that let a hermetic
/// test force the two concurrency scenarios clauses (1) and (2) bind. Default is
/// all-off (production behavior); NOTHING here is gated on ambient env.
#[derive(Default)]
pub struct RecordHooks {
    /// Invoked on the FIRST-SIGHTING path only — AFTER the "already recorded?"
    /// stream check passes and BEFORE the append write, WHILE the exclusive
    /// `observed.lock` is held. A test pauses a racer here (holding the lock) to
    /// force another racer to BLOCK on the lock, deterministically proving the
    /// check-then-act critical section is serialized (clause 1). No-op by default.
    pub before_append: Option<Box<dyn Fn() + Send>>,
    /// When `true`, the append write is forced to FAIL after the check passes,
    /// simulating a write error (or a crash) between the decision and the append.
    /// A test uses it to prove a failed append never poisons the first-sighting
    /// claim — the identity stays recordable by a later call (clause 2). Because
    /// there is no durable pre-append claim, this is exactly the crash-between-
    /// claim-and-append case: nothing durable was written, so it recovers. `false`
    /// by default.
    pub fail_append: bool,
}

/// Bounded wait to acquire `observed.lock` (a backstop against a pathological
/// live in-section holder; production sections are pure-fs and microsecond-scale,
/// and a crashed holder is OS-released). A hang returns `Err` (non-fatal), never
/// an infinite spin.
const OBSERVED_LOCK_DEADLINE_SECS: u64 = 10;
/// Poll interval while another holder owns the exclusive lock.
const OBSERVED_LOCK_POLL_MS: u64 = 5;

/// RAII guard: holds the `flock`'d lock fd for the critical section. `Drop`
/// closes the fd → the OS releases the advisory lock (also on process death).
struct ObservedLock {
    _file: std::fs::File,
}

/// Acquire the exclusive `observed.lock` (blocking, bounded). Serializes the
/// check-then-append critical section across ALL observers of one stream — the
/// check-then-act atomicity a bare `O_APPEND` cannot supply (clause 2). The lock
/// fd is `O_CLOEXEC` so a future caller wrapping observe around a spawn can never
/// leak the lock into a child (the livelock.rs P4 keystone discipline).
fn acquire_observed_lock(lock_path: &Path) -> Result<ObservedLock, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(lock_path)
        .map_err(|e| format!("could not open observed lock {}: {e}", lock_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(OBSERVED_LOCK_DEADLINE_SECS);
    loop {
        // SAFETY: flock on a valid owned fd. LOCK_EX|LOCK_NB is a non-blocking
        // try; we poll to a bounded deadline so a stuck holder never spins us
        // forever (a crashed holder is OS-released and acquires immediately).
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(ObservedLock { _file: file });
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN => {
                if Instant::now() >= deadline {
                    return Err("observed lock: timed out acquiring exclusive lock".to_string());
                }
                std::thread::sleep(Duration::from_millis(OBSERVED_LOCK_POLL_MS));
            }
            _ => return Err(format!("observed lock: flock failed: {err}")),
        }
    }
}

/// Injective, filesystem-safe encoding of the identity triple into ONE path
/// component (the marker basename). Distinct keys ALWAYS yield distinct stems, so
/// the fast-path marker can never false-hit across keys (a collision would skip a
/// genuine first sighting). NOT case-folded: a `sessionId` is case-sensitive and
/// `host`/`harness` are exact identity. `[A-Za-z0-9._-]` pass through; everything
/// else (including the `~` field separator, so it can never appear inside a
/// field) percent-escapes as uppercase `%XX`. The spec-fixed triple (short host,
/// short harness, uuid sessionId) stays well under the 255-byte filename limit.
fn encode_observed_key(host: &str, harness: &str, session_id: &str) -> String {
    format!(
        "{}~{}~{}",
        encode_key_field(host),
        encode_key_field(harness),
        encode_key_field(session_id)
    )
}

fn encode_key_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Scan `marks.jsonl` for an `observed` line whose `(host, harness, sessionId)`
/// matches the key. The AUTHORITATIVE first-sighting check (the marker is only a
/// hint). Tolerant like [`fold_marks`]: a missing file is `Ok(false)`; a torn
/// trailing line and any unparseable/non-matching line are skipped. A genuine
/// READ error (not a missing file) is `Err` — we must NOT append when we cannot
/// verify (that could duplicate), so the caller surfaces it non-fatally and the
/// key stays recordable next call.
fn observed_line_in_stream(
    marks_path: &Path,
    host: &str,
    harness: &str,
    session_id: &str,
) -> Result<bool, String> {
    let text = match std::fs::read_to_string(marks_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("could not read {}: {e}", marks_path.display())),
    };
    // Torn-trailing tolerant (same rule as fold_marks): only fold lines followed
    // by a newline; drop an unterminated trailing partial.
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        lines.pop();
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if obj.get("event").and_then(Value::as_str) != Some("observed") {
            continue;
        }
        if obj.get("host").and_then(Value::as_str) == Some(host)
            && obj.get("harness").and_then(Value::as_str) == Some(harness)
            && obj.get("sessionId").and_then(Value::as_str) == Some(session_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Best-effort creation of the fast-path marker AFTER the line has committed.
/// O_EXCL so we only ever create; an already-present marker (a concurrent racer
/// beat us to the self-heal) is fine. A write failure is swallowed: the committed
/// line remains the source of truth and a later call re-heals the marker.
fn ensure_observed_marker(claims_dir: &Path, marker: &Path) {
    if std::fs::create_dir_all(claims_dir).is_err() {
        return;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => {}
    }
}

/// **Record a first-sighting of a discovered session — hermetic form.**
///
/// The dependency-injected core of [`record_observed`]: `state_dir` is the dir
/// that holds `marks.jsonl` (so tests point it at a `tempfile` dir), `clock`
/// stamps `ts`, and `hooks` exposes the deterministic seam (force the
/// check-then-append interleave; force append-failure-after-check). Everything is
/// reachable without a real `$HOME`/`$QD_HOME`.
///
/// Appends AT MOST ONE `observed` line per `(host, harness, sessionId)` EVER in
/// `state_dir/marks.jsonl`, under concurrency. Returns:
/// - `Ok(true)`  — a new `observed` line was appended (this call won the first
///   sighting);
/// - `Ok(false)` — the key was ALREADY recorded (fast-path marker hit, or the
///   locked stream scan found the line): nothing was written;
/// - `Err(_)`    — a NON-FATAL failure (empty identity, lock/append/read error).
///   The public verb path warns and leaves its exit code unchanged; the key is
///   NOT poisoned — a later call can still record it.
///
/// See the module section above for the full correctness argument (clauses 1 & 2,
/// the marker's non-authoritative role, and the QS-8 retention boundary).
pub fn record_observed_in(
    state_dir: &Path,
    clock: &dyn Clock,
    host: &str,
    harness: &str,
    session_id: &str,
    cwd: Option<&str>,
    hooks: &RecordHooks,
) -> Result<bool, String> {
    if host.is_empty() || harness.is_empty() || session_id.is_empty() {
        return Err(
            "record_observed: empty identity component (host/harness/sessionId)".to_string(),
        );
    }

    let key = encode_observed_key(host, harness, session_id);
    let marks_path = state_dir.join("marks.jsonl");
    let claims_dir = state_dir.join("observed-claims");
    let marker = claims_dir.join(&key);

    // Fast path (lock-free): `marker present ⟹ line committed` ⇒ nothing to do.
    // This is the steady state after a key's first sighting (O(1), no lock, no
    // scan). It can only ever short-circuit to a NO-OP, never to an append.
    if marker.exists() {
        return Ok(false);
    }

    // Slow path: serialize the check-then-append under the exclusive lock.
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("could not create state dir {}: {e}", state_dir.display()))?;
    let lock_path = state_dir.join("observed.lock");
    let _guard = acquire_observed_lock(&lock_path)?;

    // AUTHORITATIVE check, under the lock: does the stream already carry the key?
    // A racer that committed while we blocked is seen here → we write nothing
    // (write-time idempotency, clause 1). This ALSO recovers a lost marker: the
    // line exists, we just re-heal the marker and no-op.
    if observed_line_in_stream(&marks_path, host, harness, session_id)? {
        ensure_observed_marker(&claims_dir, &marker);
        return Ok(false);
    }

    // First sighting. The test seam pauses here (holding the lock) to force
    // another racer to block on the lock — proving serialization.
    if let Some(hook) = &hooks.before_append {
        hook();
    }

    // Forced append failure (test): return WITHOUT writing a line or a marker.
    // Because no durable claim precedes the append, the key stays recordable — a
    // later call re-appends (clause 2, and the crash-between-claim-and-append case).
    if hooks.fail_append {
        return Err("record_observed: append forced-failed (test seam)".to_string());
    }

    // Self-delimiting sub-PIPE_BUF single O_APPEND write (`\n{line}\n`): the leading
    // newline guarantees our record is INDEPENDENTLY READABLE even when the shared
    // stream ends in a torn tail left by a non-observed writer (F-DEOBS-1), so the
    // `marker ⟹ readable line` invariant holds. On failure NO marker is written, so
    // the key is left recordable for the next call (never poisoned).
    let line = build_observed_line(
        &epoch_ms_to_iso(clock.now_ms()),
        host,
        harness,
        session_id,
        cwd,
    );
    append_observed_self_delimited(&marks_path, &line)?;

    // Commit the fast-path marker AFTER the line is durable (best-effort).
    ensure_observed_marker(&claims_dir, &marker);
    Ok(true)
}

/// **Record a first-sighting of a discovered session (spec S3.2).**
///
/// A clean, best-effort recorder for the downstream consumer (DP's scan across
/// the fog line). Appends AT MOST ONE `observed` line per identity key
/// `(host, harness, sessionId)` EVER, in this host's `marks.jsonl` stream, under
/// concurrency — see the module section above for the mechanism and the
/// correctness argument.
///
/// - **Identity-facts-only:** records `host`, `harness`, `sessionId` (+ optional
///   `cwd`). NO pid, NO liveness, NO status. `host` is explicit in the key
///   (cross-host non-collision).
/// - **First-sighting semantics:** a thousand `ls` runs append ZERO lines after
///   the first sighting of each key.
/// - **Non-fatal contract:** resolves the marks path via the injected `Env`
///   (QD_HOME-honoring). `Err` carries a human reason for the caller to WARN
///   about; the caller MUST NOT change its exit code on failure. A failure never
///   poisons the first-sighting claim.
/// - **No registry row:** this path creates NO pid-keyed live-registry entry
///   (never calls `registry::write_entry`/`set_status`/`claim_name`).
/// - **Retention boundary (QS-8):** the dedup memory lives in the stream AND the
///   sibling `observed-claims/` markers — rotating `marks.jsonl` must also clear
///   `observed-claims/`, else re-observation after stream loss silently can't
///   happen.
pub fn record_observed(
    env: &dyn Env,
    clock: &dyn Clock,
    host: &str,
    harness: &str,
    session_id: &str,
    cwd: Option<&str>,
) -> Result<(), String> {
    let marks = marks_path(env)
        .ok_or_else(|| "HOME is not set — cannot resolve the state dir".to_string())?;
    let state_dir = marks
        .parent()
        .ok_or_else(|| "marks path has no parent state dir".to_string())?;
    record_observed_in(
        state_dir,
        clock,
        host,
        harness,
        session_id,
        cwd,
        &RecordHooks::default(),
    )
    .map(|_| ())
}

// ===========================================================================
// Snapshot fold (spec §4.3) — pure, tolerant, last-write-wins
// ===========================================================================

/// One folded telemetry record for a session. Carries only the additive A6
/// surfacing fields. Both keys (sessionId, name) may have produced it; the join
/// uses sessionId-first then name-fallback (see [`SnapshotMap::lookup`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FoldedSession {
    pub backend: Option<String>,
    pub spawned_by: Option<String>,
}

impl FoldedSession {
    fn is_empty(&self) -> bool {
        self.backend.is_none() && self.spawned_by.is_none()
    }
}

/// The fold result: two independent indexes built from the SAME create events —
/// one keyed by sessionId, one keyed by name. The render seam looks up
/// sessionId-first, name-fallback (spec §4.3, red-team F6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotMap {
    by_session_id: HashMap<String, FoldedSession>,
    by_name: HashMap<String, FoldedSession>,
}

impl SnapshotMap {
    /// Look up a session's folded telemetry. Join precedence (F6): an exact
    /// sessionId match wins; the name-keyed fallback is consulted ONLY when no
    /// sessionId-keyed entry exists for `session_id`.
    ///
    /// NAMED v1 LIMITATION (spec §4.3): a name reused across time (tombstoned +
    /// live) can mis-attribute via the name fallback until a sessionId-keyed line
    /// exists. Pinned by a deterministic unit test below.
    pub fn lookup(&self, session_id: &str, name: Option<&str>) -> Option<&FoldedSession> {
        if !session_id.is_empty() {
            if let Some(f) = self.by_session_id.get(session_id) {
                return Some(f);
            }
        }
        name.and_then(|n| self.by_name.get(n))
    }

    /// True when the fold yielded nothing (missing/empty/all-skipped input). The
    /// render seam treats this identically to `None` → today's bytes.
    pub fn is_empty(&self) -> bool {
        self.by_session_id.is_empty() && self.by_name.is_empty()
    }
}

/// Fold the `marks.jsonl` stream into a [`SnapshotMap`]. PURE over the text.
///
/// Tolerance (spec §4.3, 0b dirty-state lesson):
/// - a torn TRAILING line (no final newline / truncated JSON) is IGNORED — only
///   whole, parseable lines contribute;
/// - non-object lines, mark lines (top-level `"payload"`, no top-level
///   `"event"`), and unknown event kinds are SKIPPED;
/// - last-write-wins per key for `backend`/`spawnedBy`.
///
/// NOTHING here unwraps on external data — every parse degrades to a skip.
pub fn fold_marks(text: &str) -> SnapshotMap {
    let mut map = SnapshotMap::default();

    // A torn trailing line: if the text does not end in '\n', the final segment
    // may be a partial write. We only fold lines that are followed by a newline;
    // the trailing partial (if any) is dropped. (A clean final line WITH a
    // trailing newline produces an empty trailing segment from split, which is
    // skipped as non-JSON anyway.)
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        // Drop the unterminated trailing segment (torn write).
        lines.pop();
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // unparseable → skip (tolerant)
        };
        let Value::Object(obj) = v else {
            continue; // non-object line → skip
        };
        // TOP-LEVEL "event" distinguishes an engine event line from a mark line.
        // A mark line has "payload" and no top-level "event"; an "event" key
        // nested inside a payload object is invisible here (we read obj["event"]
        // only). No collision.
        let Some(Value::String(kind)) = obj.get("event") else {
            continue; // mark line or eventless line → skip
        };
        if kind != "create" {
            // Only `create` events carry backend/spawnedBy. `invoked` lines (and any
            // future/unknown kind) are pure occurrence records → skipped by the fold.
            continue;
        }

        let backend = str_field(&obj, "backend");
        let spawned_by = str_field(&obj, "spawnedBy");
        // A create event with neither field is SKIPPED entirely (we only store
        // when there is signal): it neither keys the session nor contributes to
        // render. Consequence: such a session still falls through to the name
        // fallback on lookup — acceptable, an empty record would render the same
        // nothing (lead review nit, integration commit).
        let folded = FoldedSession {
            backend,
            spawned_by,
        };
        if folded.is_empty() {
            continue;
        }

        // Last-write-wins per key. Index by sessionId AND name when present.
        if let Some(sid) = str_field(&obj, "sessionId") {
            map.by_session_id.insert(sid, folded.clone());
        }
        if let Some(name) = str_field(&obj, "name") {
            map.by_name.insert(name, folded);
        }
    }

    map
}

/// Best-effort fold from the marks file resolved via `env`. A missing/unreadable
/// file → an EMPTY map (never an error). The render seam treats empty ≡ today's
/// bytes. NOTHING here can fail the calling verb.
pub fn fold_from_env(env: &dyn Env) -> SnapshotMap {
    let Some(path) = marks_path(env) else {
        return SnapshotMap::default();
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    fold_marks(&text)
}

/// Pull a non-empty string field from a JSON object, else `None`. A wrong-typed
/// field (number, bool, ...) degrades to `None` (tolerant).
fn str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{FixedClock, MapEnv};
    use crate::exec::ScriptedExec;
    use serde_json::json;
    use tempfile::tempdir;

    // --- ps_ppid / find_caller_session over the Exec seam (F11 new unit) ---

    #[test]
    fn ps_ppid_parses_through_injected_exec() {
        // A MapExec/fake (ScriptedExec) returns canned `ps` output; ps_ppid parses
        // the trimmed integer.
        let exec = ScriptedExec::new().on("ps", &["-o"], Some(0), " 4242 \n", "");
        assert_eq!(ps_ppid(&exec, 1234), Some(4242));
    }

    #[test]
    fn ps_ppid_none_on_nonnumeric() {
        let exec = ScriptedExec::new().on("ps", &["-o"], Some(0), "not-a-number", "");
        assert_eq!(ps_ppid(&exec, 1234), None);
    }

    #[test]
    fn find_caller_session_returns_none_when_no_entry_and_walk_stops() {
        // ps returns 1 (init) → the walk stops; no registry dir → None. Proves the
        // hoisted walk drives through the injected Exec, not a hard-coded RealExec.
        let dir = tempdir().unwrap();
        let paths = QdPaths::from_home(dir.path());
        let exec = ScriptedExec::new().on("ps", &["-o"], Some(0), "1", "");
        assert_eq!(find_caller_session(&paths, &exec), None);
    }

    // --- create / invoked line builders: exact key order + omission ---

    #[test]
    fn create_line_full_key_order() {
        let ev = CreateEvent {
            name: "child".into(),
            session_id: Some("sid-1".into()),
            spawned_by: Some("parent".into()),
            spawned_by_session_id: Some("sid-0".into()),
            backend: Some("ccr-3456".into()),
        };
        let line = build_create_line("2026-06-05T10:00:00.000Z", &ev);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "ts",
                "event",
                "name",
                "sessionId",
                "spawnedBy",
                "spawnedBySessionId",
                "backend"
            ]
        );
        assert_eq!(parsed["event"], json!("create"));
        assert_eq!(parsed["backend"], json!("ccr-3456"));
    }

    #[test]
    fn create_line_omits_absent_optionals_never_empty_strings() {
        // Only name present → ts, event, name and nothing else (no empty strings).
        let ev = CreateEvent {
            name: "child".into(),
            ..Default::default()
        };
        let line = build_create_line("t", &ev);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["ts", "event", "name"]);
    }

    #[test]
    fn create_line_treats_empty_string_optionals_as_absent() {
        let ev = CreateEvent {
            name: "child".into(),
            session_id: Some(String::new()),
            spawned_by: Some(String::new()),
            ..Default::default()
        };
        let line = build_create_line("t", &ev);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["ts", "event", "name"],
            "empty-string optionals dropped"
        );
    }

    #[test]
    fn invoked_line_key_order_and_omission() {
        let line = build_invoked_line("t", "send", Some("sid-1"), Some("alpha"));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["ts", "event", "verb", "sessionId", "name"]);

        // name omitted when None.
        let line2 = build_invoked_line("t", "mark", Some("sid-1"), None);
        let parsed2: Value = serde_json::from_str(&line2).unwrap();
        let keys2: Vec<&str> = parsed2
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys2, vec!["ts", "event", "verb", "sessionId"]);
    }

    // --- append + non-fatal API ---

    #[test]
    fn append_create_and_invoked_are_jsonl() {
        let home = tempdir().unwrap();
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".into(), home.path().to_string_lossy().into_owned());
        let clock = FixedClock(1_717_530_000_000);

        append_create_event(
            &env,
            &clock,
            &CreateEvent {
                name: "child".into(),
                session_id: Some("sid-1".into()),
                spawned_by: Some("parent".into()),
                backend: Some("ccr-a".into()),
                ..Default::default()
            },
        )
        .unwrap();
        append_invoked(&env, &clock, "send", Some("sid-1"), Some("child")).unwrap();

        let path = home.path().join(".quorum/dispatch/state/marks.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let c: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(c["event"], json!("create"));
        let u: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(u["event"], json!("invoked"));
        assert!(text.ends_with('\n'));
    }

    // --- fold dirty-state matrix (spec §7 G-A3 / G-A6) ---

    #[test]
    fn fold_missing_or_empty_is_empty_map() {
        assert!(fold_marks("").is_empty());
        assert!(fold_marks("\n").is_empty());
        // fold_from_env with no file → empty.
        let home = tempdir().unwrap();
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".into(), home.path().to_string_lossy().into_owned());
        assert!(fold_from_env(&env).is_empty());
    }

    #[test]
    fn fold_ignores_torn_trailing_line() {
        // Two whole lines + a torn (unterminated, truncated) third.
        let whole = build_create_line(
            "t1",
            &CreateEvent {
                name: "a".into(),
                session_id: Some("sid-a".into()),
                backend: Some("be-a".into()),
                ..Default::default()
            },
        );
        let text = format!("{whole}\n{{\"ts\":\"t2\",\"event\":\"crea"); // torn, no newline
        let map = fold_marks(&text);
        // The whole line folded; the torn tail was ignored (no panic).
        assert_eq!(
            map.lookup("sid-a", None).unwrap().backend.as_deref(),
            Some("be-a")
        );
    }

    #[test]
    fn fold_skips_mark_lines_and_unknown_kinds_and_invoked() {
        // Interleaved: a mark line (payload, no event), an invoked line, an unknown
        // event kind, and a real create — only the create folds.
        let mark =
            json!({"ts":"t","sessionId":"sid-x","payload":{"event":"create","backend":"SPOOF"}})
                .to_string();
        let invoked = build_invoked_line("t", "send", Some("sid-x"), Some("x"));
        let unknown =
            json!({"ts":"t","event":"frobnicate","sessionId":"sid-x","backend":"NOPE"}).to_string();
        let create = build_create_line(
            "t",
            &CreateEvent {
                name: "real".into(),
                session_id: Some("sid-real".into()),
                backend: Some("be-real".into()),
                spawned_by: Some("p".into()),
                ..Default::default()
            },
        );
        let text = format!("{mark}\n{invoked}\n{unknown}\n{create}\n");
        let map = fold_marks(&text);
        // The mark line's INNER "event":"create" + "backend":"SPOOF" did NOT leak.
        assert!(
            map.lookup("sid-x", Some("x")).is_none(),
            "mark/invoked/unknown contributed nothing"
        );
        let f = map.lookup("sid-real", None).unwrap();
        assert_eq!(f.backend.as_deref(), Some("be-real"));
        assert_eq!(f.spawned_by.as_deref(), Some("p"));
    }

    #[test]
    fn fold_last_write_wins_per_key() {
        let first = build_create_line(
            "t1",
            &CreateEvent {
                name: "a".into(),
                session_id: Some("sid-a".into()),
                backend: Some("be-old".into()),
                ..Default::default()
            },
        );
        let second = build_create_line(
            "t2",
            &CreateEvent {
                name: "a".into(),
                session_id: Some("sid-a".into()),
                backend: Some("be-new".into()),
                ..Default::default()
            },
        );
        let map = fold_marks(&format!("{first}\n{second}\n"));
        assert_eq!(
            map.lookup("sid-a", None).unwrap().backend.as_deref(),
            Some("be-new")
        );
    }

    #[test]
    fn fold_join_precedence_sessionid_first_then_name() {
        // A create keyed by BOTH sid and name; a SECOND create keyed by name only
        // (different name) — the lookup prefers the sid-keyed record.
        let by_both = build_create_line(
            "t1",
            &CreateEvent {
                name: "alpha".into(),
                session_id: Some("sid-1".into()),
                backend: Some("be-sid".into()),
                ..Default::default()
            },
        );
        let map = fold_marks(&format!("{by_both}\n"));
        // sid present + matches → sid record (even if a different name is passed).
        assert_eq!(
            map.lookup("sid-1", Some("alpha"))
                .unwrap()
                .backend
                .as_deref(),
            Some("be-sid")
        );
        // sid present but UNKNOWN → fall back to the name index.
        assert_eq!(
            map.lookup("sid-unknown", Some("alpha"))
                .unwrap()
                .backend
                .as_deref(),
            Some("be-sid")
        );
        // neither key matches → None.
        assert!(map.lookup("sid-unknown", Some("other")).is_none());
    }

    #[test]
    fn fold_name_reuse_misattributes_via_name_fallback_named_limitation() {
        // SPEC §4.3 NAMED v1 LIMITATION pinned deterministically:
        // a tombstoned session "reused-name" wrote a create with backend be-OLD
        // keyed by name (no sessionId captured at the time). A NEW live session
        // reuses the same name but has a DIFFERENT sessionId and has NOT yet
        // emitted a sessionId-keyed create line. Looking it up by its real sessionId
        // (unknown to the fold) falls back to the NAME index → mis-attributes the
        // OLD backend. This is the documented behavior, not a bug to hide.
        let old = build_create_line(
            "t-old",
            &CreateEvent {
                name: "reused-name".into(),
                backend: Some("be-OLD".into()),
                ..Default::default() // no sessionId — name-only keying
            },
        );
        let map = fold_marks(&format!("{old}\n"));
        // The NEW session's real sessionId is not in the fold; name fallback hits OLD.
        let f = map.lookup("sid-NEW-live", Some("reused-name")).unwrap();
        assert_eq!(
            f.backend.as_deref(),
            Some("be-OLD"),
            "name reuse mis-attributes until a sessionId-keyed line exists (named v1 limitation)"
        );
    }

    #[test]
    fn fold_inner_event_key_in_payload_does_not_collide() {
        // OPAQUE-PAYLOAD property (G-A6): a mark whose payload object itself
        // contains an "event":"create" key + a "backend" key MUST NOT be read as an
        // engine create event. The fold reads only the TOP-LEVEL event key.
        let mark = json!({
            "ts":"t",
            "sessionId":"sid-z",
            "payload":{"event":"create","backend":"INJECT","spawnedBy":"INJECT"}
        })
        .to_string();
        let map = fold_marks(&format!("{mark}\n"));
        assert!(
            map.lookup("sid-z", None).is_none(),
            "inner event key stayed inside payload"
        );
        assert!(map.is_empty());
    }

    #[test]
    fn fold_tolerates_wrong_typed_event_fields() {
        // A create line with a numeric backend / boolean spawnedBy degrades those
        // fields to None (no panic), but the line is still a create.
        let line = json!({"ts":"t","event":"create","name":"a","sessionId":"sid-a","backend":123,"spawnedBy":true}).to_string();
        let map = fold_marks(&format!("{line}\n"));
        // Both fields degraded → the record is empty → not stored.
        assert!(map.lookup("sid-a", None).is_none());
    }

    // =======================================================================
    // DE-observed — record_observed + `observed` event kind (spec S3.2)
    // =======================================================================

    /// Count `observed` lines in a marks file whose (host,harness,sessionId)
    /// match — the physical duplicate detector the write-time clause (1) binds.
    fn count_observed(marks_path: &Path, host: &str, harness: &str, sid: &str) -> usize {
        let Ok(text) = std::fs::read_to_string(marks_path) else {
            return 0;
        };
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
            .filter(|v| {
                v.get("event").and_then(Value::as_str) == Some("observed")
                    && v.get("host").and_then(Value::as_str) == Some(host)
                    && v.get("harness").and_then(Value::as_str) == Some(harness)
                    && v.get("sessionId").and_then(Value::as_str) == Some(sid)
            })
            .count()
    }

    // --- builder: exact key order + cwd omission ---

    #[test]
    fn observed_line_key_order_and_cwd_omission() {
        let line = build_observed_line(
            "2026-07-15T10:00:00.000Z",
            "qrmoh",
            "claude",
            "sid-uuid-1",
            Some("/work/proj"),
        );
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["ts", "event", "host", "harness", "sessionId", "cwd"]
        );
        assert_eq!(parsed["event"], json!("observed"));
        // Identity-facts-only: no pid/status/liveness keys.
        assert!(parsed.get("pid").is_none());
        assert!(parsed.get("status").is_none());

        // cwd omitted (None) and empty-string treated as absent.
        let no_cwd = build_observed_line("t", "h", "claude", "sid", None);
        let keys2: Vec<String> = serde_json::from_str::<Value>(&no_cwd)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys2, vec!["ts", "event", "host", "harness", "sessionId"]);
        let empty_cwd = build_observed_line("t", "h", "claude", "sid", Some(""));
        assert!(!empty_cwd.contains("cwd"), "empty-string cwd dropped");
    }

    // --- deliverable #3: readers skip `observed` like any unknown (non-create) kind ---

    #[test]
    fn fold_skips_observed_lines() {
        // An `observed` line carries no backend/spawnedBy and is NOT a `create`,
        // so the fold's `kind != "create"` branch skips it (inert). Pinned so the
        // reader-tolerance property is explicit, alongside a real create that DOES
        // fold — proving the observed line neither contributes nor blocks.
        let observed = build_observed_line("t", "qrmoh", "claude", "sid-obs", Some("/w"));
        let create = build_create_line(
            "t",
            &CreateEvent {
                name: "real".into(),
                session_id: Some("sid-obs".into()),
                backend: Some("be-real".into()),
                ..Default::default()
            },
        );
        let map = fold_marks(&format!("{observed}\n{create}\n"));
        // The observed line contributed nothing; the create still folded.
        let f = map.lookup("sid-obs", None).unwrap();
        assert_eq!(f.backend.as_deref(), Some("be-real"));
        // An observed line ALONE folds to an empty map (no create anywhere).
        assert!(fold_marks(&format!("{observed}\n")).is_empty());
    }

    // --- record_observed_in: first-sighting appends once, then no-ops ---

    #[test]
    fn record_observed_first_sighting_appends_once_then_noops() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1_752_573_600_000);
        let marks = dir.path().join("marks.jsonl");
        let hooks = RecordHooks::default();

        // First sighting → appended.
        assert_eq!(
            record_observed_in(
                dir.path(),
                &clock,
                "qrmoh",
                "claude",
                "sid-1",
                Some("/w"),
                &hooks
            ),
            Ok(true)
        );
        // The fast-path marker was committed.
        assert!(dir
            .path()
            .join("observed-claims")
            .join("qrmoh~claude~sid-1")
            .exists());

        // A thousand re-sightings append ZERO further lines (all fast-path no-ops).
        for _ in 0..1000 {
            assert_eq!(
                record_observed_in(
                    dir.path(),
                    &clock,
                    "qrmoh",
                    "claude",
                    "sid-1",
                    Some("/w"),
                    &hooks
                ),
                Ok(false)
            );
        }
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-1"), 1);
    }

    // --- record_observed_in: distinct keys each record; host is IN the key ---

    #[test]
    fn record_observed_distinct_keys_each_record_and_host_in_key() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1);
        let h = RecordHooks::default();
        let marks = dir.path().join("marks.jsonl");

        // Same native sessionId on TWO hosts → two legitimate facts (host in key).
        assert_eq!(
            record_observed_in(dir.path(), &clock, "hostA", "claude", "sid-x", None, &h),
            Ok(true)
        );
        assert_eq!(
            record_observed_in(dir.path(), &clock, "hostB", "claude", "sid-x", None, &h),
            Ok(true)
        );
        // Different harness on the same host+sid → a distinct fact too.
        assert_eq!(
            record_observed_in(dir.path(), &clock, "hostA", "codex", "sid-x", None, &h),
            Ok(true)
        );
        assert_eq!(count_observed(&marks, "hostA", "claude", "sid-x"), 1);
        assert_eq!(count_observed(&marks, "hostB", "claude", "sid-x"), 1);
        assert_eq!(count_observed(&marks, "hostA", "codex", "sid-x"), 1);
    }

    // NOTE (de-observed test hygiene, fix-cycle): the executor's
    // `record_observed_concurrent_first_sighting_writes_exactly_one_line` was
    // REMOVED here. It used `thread::sleep(80ms)` to let racer B reach the lock —
    // its reversion-redness was timing-gated (a sleep-race), which violates the
    // spec's "deterministic, not a sleep-race" clause for the interleave. Its
    // deterministic lock-exclusion AND its real-second-racer realism are both
    // subsumed by `deobs_interleave_lock_serializes_check_then_act` below (a
    // sleep-free LOCK_NB probe + a real racer B). Invariant after this deletion:
    // NO test claims to force the check-then-act interleave via a sleep-race.

    // --- clause (2): a failed append after the check never poisons the claim ---
    //
    // Forces append-failure-after-check via the hook, then a NORMAL call records
    // the SAME key. If the mechanism used a durable pre-append claim (the trap),
    // the failed attempt would leave a permanent marker and the second call would
    // write nothing (0 lines) — this test would fail. Here the claim IS the line:
    // a failed append leaves nothing, so the key stays recordable.
    #[test]
    fn record_observed_failed_append_does_not_poison_claim() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1);
        let marks = dir.path().join("marks.jsonl");

        let fail = RecordHooks {
            before_append: None,
            fail_append: true,
        };
        let err = record_observed_in(dir.path(), &clock, "qrmoh", "claude", "sid-2", None, &fail);
        assert!(err.is_err(), "forced append failure surfaces as Err");
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-2"), 0);
        // No poisoning marker was left behind.
        assert!(!dir
            .path()
            .join("observed-claims")
            .join("qrmoh~claude~sid-2")
            .exists());

        // A later NORMAL call still records the identity (recoverable together).
        let ok = RecordHooks::default();
        assert_eq!(
            record_observed_in(dir.path(), &clock, "qrmoh", "claude", "sid-2", None, &ok),
            Ok(true)
        );
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-2"), 1);
    }

    // --- deliverable #4: record_observed writes NO registry row ---

    #[test]
    fn record_observed_writes_no_registry_row() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1);
        let h = RecordHooks::default();
        record_observed_in(
            dir.path(),
            &clock,
            "qrmoh",
            "claude",
            "sid-3",
            Some("/w"),
            &h,
        )
        .unwrap();

        // The ONLY artifacts under the state dir are marks.jsonl, observed.lock,
        // and the observed-claims/ marker. Specifically: no pid-keyed registry
        // row (`*.json`) and no name-claim file (`*.claim`) — this path never
        // enters the live registry.
        let mut files = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        walk(dir.path(), &mut files);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".json")),
            "no registry row written: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with(".claim")),
            "no name-claim written: {names:?}"
        );
        // Positively: exactly the observe artifacts exist.
        assert!(dir.path().join("marks.jsonl").exists());
        assert!(dir.path().join("observed.lock").exists());
        assert!(dir
            .path()
            .join("observed-claims")
            .join("qrmoh~claude~sid-3")
            .exists());
    }

    // --- empty identity component is a non-fatal Err, writes nothing ---

    #[test]
    fn record_observed_rejects_empty_identity() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(1);
        let h = RecordHooks::default();
        assert!(record_observed_in(dir.path(), &clock, "", "claude", "sid", None, &h).is_err());
        assert!(record_observed_in(dir.path(), &clock, "host", "", "sid", None, &h).is_err());
        assert!(record_observed_in(dir.path(), &clock, "host", "claude", "", None, &h).is_err());
        // Nothing was written for any of them.
        assert!(!dir.path().join("marks.jsonl").exists());
    }

    // --- public record_observed: resolves state dir via Env (QD_HOME-honoring) ---

    #[test]
    fn record_observed_public_path_writes_under_state_dir() {
        let home = tempdir().unwrap();
        let mut env = MapEnv::default();
        env.vars
            .insert("HOME".into(), home.path().to_string_lossy().into_owned());
        let clock = FixedClock(1_752_573_600_000);

        record_observed(&env, &clock, "qrmoh", "claude", "sid-pub", Some("/w")).unwrap();
        let marks = home.path().join(".quorum/dispatch/state/marks.jsonl");
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-pub"), 1);
        // Idempotent on the public path too.
        record_observed(&env, &clock, "qrmoh", "claude", "sid-pub", Some("/w")).unwrap();
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-pub"), 1);
    }

    // --- encode_observed_key is injective across field boundaries ---

    #[test]
    fn encode_observed_key_is_injective_at_separators() {
        // The `~` joiner cannot be forged from within a field (it escapes to %7E),
        // so these three DISTINCT triples yield three DISTINCT stems.
        let a = encode_observed_key("a~b", "c", "d");
        let b = encode_observed_key("a", "b~c", "d");
        let c = encode_observed_key("a", "b", "c~d");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // Path-separators / traversal bytes escape (never a second component).
        let enc = encode_observed_key("../x", "cl/a", "s");
        assert!(!enc.contains('/'));
        assert!(!enc.contains("..") || !enc.contains('/'));
    }

    // =======================================================================
    // DE-observed INDEPENDENT conformance/seam tests (author: de-observed
    // conformance seat; commissioned by de-observed-coord). Derived from the
    // SPEC (S3.2 clauses 1 & 2), NOT from the executor's reasoning. Each is
    // hermetic (per-test tempdir, injected clock/dir, no ambient QD_*) and
    // sleep-free where it forces the property, so a compile-saturated box
    // cannot invalidate the signal.
    //
    // Reversion oracle (mandatory, reproducible on lima):
    //   R1 — neuter the flock serialization in `acquire_observed_lock` (make it
    //        acquire unconditionally, e.g. `return Ok(ObservedLock { _file: file });`
    //        immediately after opening the file, skipping the flock loop). Then
    //        `deobs_interleave_lock_serializes_check_then_act` goes RED: the
    //        LOCK_NB probe below ACQUIRES while racer A is in-section, so the
    //        `rc == -1` assertion fails. Restore ⇒ GREEN.
    //   R2 — move `ensure_observed_marker(&claims_dir, &marker);` from AFTER the
    //        append to BEFORE the `if hooks.fail_append` early-return (the
    //        "O_EXCL claim marker" trap). Then
    //        `deobs_failed_append_leaves_identity_recordable` goes RED: the
    //        forced-fail call leaves a marker, the later normal call fast-paths
    //        to Ok(false), and `assert_eq!(.., Ok(true))` fails. Restore ⇒ GREEN.
    // =======================================================================

    /// Clause 1 — the LOAD-BEARING deterministic interleave test.
    ///
    /// Independent of the executor's `..._writes_exactly_one_line`, which times
    /// racer B's arrival with an 80ms sleep (its reversion-redness is thus
    /// timing-gated). Here mutual exclusion is proven WITHOUT a sleep: while
    /// racer A is pinned IN-SECTION (lock held, post-check, pre-append) via the
    /// `before_append` hook, an independent non-blocking `flock(LOCK_EX|LOCK_NB)`
    /// on the SAME `observed.lock` MUST be denied. `flock` treats separate open
    /// descriptions independently, so this conflicts even in-process — the exact
    /// probe that distinguishes genuine check-then-act-under-the-lock from
    /// reader-side dedup in disguise. A real racer B then confirms the loser
    /// returns `Ok(false)` and writes nothing.
    #[test]
    fn deobs_interleave_lock_serializes_check_then_act() {
        use std::os::unix::io::AsRawFd;
        use std::sync::mpsc;
        use std::thread;

        let dir = tempdir().unwrap();
        let marks = dir.path().join("marks.jsonl");
        let lock_path = dir.path().join("observed.lock");

        let (reached_tx, reached_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<(&'static str, Result<bool, String>)>();

        // Racer A: pins itself IN-SECTION (lock held, post-check, pre-append).
        let dir_a = dir.path().to_path_buf();
        let res_tx_a = res_tx.clone();
        let a = thread::spawn(move || {
            let clock = FixedClock(10);
            let hook = move || {
                reached_tx.send(()).unwrap(); // "I hold the lock, before the append."
                go_rx.recv().unwrap(); // Stay in-section until released.
            };
            let hooks = RecordHooks {
                before_append: Some(Box::new(hook)),
                fail_append: false,
            };
            let r = record_observed_in(&dir_a, &clock, "qrmoh", "claude", "sid-det", None, &hooks);
            res_tx_a.send(("A", r)).unwrap();
        });

        // Deterministically wait until A is pinned in the critical section.
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("racer A never reached the in-section hook");

        // (i) CHECK-THEN-*ACT* WINDOW: A passed the check but has NOT appended.
        assert_eq!(
            count_observed(&marks, "qrmoh", "claude", "sid-det"),
            0,
            "A must be pre-append while pinned in-section"
        );

        // (ii) MUTUAL EXCLUSION, sleep-free: an independent attempt to enter the
        // critical region MUST be denied while A holds the lock. If the lock were
        // neutered (R1), this ACQUIRES and the assertion fails → the test is
        // sensitive to the exact mechanism, not passing by construction.
        let probe = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let rc = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        let os_err = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            rc, -1,
            "probe acquired observed.lock while A holds it — section is NOT serialized"
        );
        assert!(
            matches!(os_err, Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN),
            "expected EWOULDBLOCK/EAGAIN from the contended lock, got {os_err:?}"
        );
        drop(probe);

        // Racer B: a REAL second racer for the SAME key. It blocks on the lock
        // (or arrives after release); either way its under-lock scan finds A's
        // committed line and it writes nothing. Outcome is deterministic
        // regardless of B's arrival timing (no sleep needed).
        let dir_b = dir.path().to_path_buf();
        let res_tx_b = res_tx.clone();
        let b = thread::spawn(move || {
            let clock = FixedClock(20);
            let hooks = RecordHooks::default();
            let r = record_observed_in(&dir_b, &clock, "qrmoh", "claude", "sid-det", None, &hooks);
            res_tx_b.send(("B", r)).unwrap();
        });

        // Release A → it appends and drops the lock.
        go_tx.send(()).unwrap();

        // Collect both results under a bounded deadline (never spin).
        let mut got: HashMap<&'static str, Result<bool, String>> = HashMap::new();
        for _ in 0..2 {
            let (who, r) = res_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("a racer hung (no result within 10s)");
            got.insert(who, r);
        }
        a.join().unwrap();
        b.join().unwrap();

        // Exactly one physical line; A won, B detected "already recorded".
        assert_eq!(
            count_observed(&marks, "qrmoh", "claude", "sid-det"),
            1,
            "exactly one observed line for the key"
        );
        assert_eq!(got.get("A"), Some(&Ok(true)), "A won the first sighting");
        assert_eq!(
            got.get("B"),
            Some(&Ok(false)),
            "B saw A's line and wrote nothing"
        );
    }

    /// Clause 2 — append-failure-after-check ⇒ identity stays recordable.
    ///
    /// Independent of the executor's `..._does_not_poison_claim`. Forces the
    /// append to fail AFTER the check (`fail_append`), asserts NO line and NO
    /// durable marker were left, then proves a later normal call still records
    /// the SAME identity (`Ok(true)`, one line) — the claim was never poisoned
    /// because the line IS the claim (no O_EXCL marker precedes the append).
    /// Goes RED under R2 (marker moved before the append).
    #[test]
    fn deobs_failed_append_leaves_identity_recordable() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(7);
        let marks = dir.path().join("marks.jsonl");
        let marker = dir
            .path()
            .join("observed-claims")
            .join("h~claude~sid-clause2");

        // Force append-failure AFTER the check passes.
        let fail = RecordHooks {
            before_append: None,
            fail_append: true,
        };
        let r = record_observed_in(
            dir.path(),
            &clock,
            "h",
            "claude",
            "sid-clause2",
            None,
            &fail,
        );
        assert!(
            r.is_err(),
            "forced append failure surfaces as a non-fatal Err"
        );
        assert_eq!(
            count_observed(&marks, "h", "claude", "sid-clause2"),
            0,
            "a failed append writes no line"
        );
        assert!(
            !marker.exists(),
            "NO durable pre-append claim/marker is left behind (identity not poisoned)"
        );

        // The identity is NOT permanently unrecordable: a later normal call records it.
        let ok = RecordHooks::default();
        assert_eq!(
            record_observed_in(dir.path(), &clock, "h", "claude", "sid-clause2", None, &ok),
            Ok(true),
            "identity stays recordable after a failed append (claim never poisoned)"
        );
        assert_eq!(count_observed(&marks, "h", "claude", "sid-clause2"), 1);
        assert!(
            marker.exists(),
            "the marker is committed only AFTER a successful append"
        );
    }

    /// First-sighting volume — the thousand-`ls` property. N repeated calls on
    /// one key ⇒ exactly one line; many DISTINCT keys ⇒ one line each, with no
    /// cross-key suppression, and the original key stays at exactly one after all
    /// the distinct-key traffic.
    #[test]
    fn deobs_first_sighting_volume_exactly_one_line_per_key() {
        let dir = tempdir().unwrap();
        let clock = FixedClock(3);
        let h = RecordHooks::default();
        let marks = dir.path().join("marks.jsonl");

        // N repeated calls, SAME key ⇒ exactly one line.
        assert_eq!(
            record_observed_in(
                dir.path(),
                &clock,
                "qrmoh",
                "claude",
                "sid-vol",
                Some("/w"),
                &h
            ),
            Ok(true)
        );
        for _ in 0..500 {
            assert_eq!(
                record_observed_in(
                    dir.path(),
                    &clock,
                    "qrmoh",
                    "claude",
                    "sid-vol",
                    Some("/w"),
                    &h
                ),
                Ok(false)
            );
        }
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-vol"), 1);

        // DISTINCT keys ⇒ one line each, no cross-key suppression.
        for i in 0..50 {
            let sid = format!("sid-{i}");
            assert_eq!(
                record_observed_in(dir.path(), &clock, "qrmoh", "claude", &sid, None, &h),
                Ok(true),
                "distinct key {sid} is its own first sighting"
            );
            assert_eq!(
                record_observed_in(dir.path(), &clock, "qrmoh", "claude", &sid, None, &h),
                Ok(false),
                "immediate re-sight of {sid} no-ops"
            );
            assert_eq!(count_observed(&marks, "qrmoh", "claude", &sid), 1);
        }
        // The original volume key is untouched by the distinct-key traffic.
        assert_eq!(count_observed(&marks, "qrmoh", "claude", "sid-vol"), 1);
    }

    /// No registry row — the identity-fact-only property. `record_observed_in`
    /// must touch NEITHER a sibling `sessions/` registry dir NOR write any
    /// pid-keyed row (`*.json`) or name-claim (`*.claim`) anywhere.
    #[test]
    fn deobs_creates_no_registry_row_sibling_registry_untouched() {
        let root = tempdir().unwrap();
        let state_dir = root.path().join("state");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let clock = FixedClock(1);
        let h = RecordHooks::default();

        record_observed_in(
            &state_dir,
            &clock,
            "qrmoh",
            "claude",
            "sid-reg",
            Some("/w"),
            &h,
        )
        .unwrap();

        // The sibling registry dir is untouched: still empty.
        let entries: Vec<_> = std::fs::read_dir(&sessions_dir)
            .unwrap()
            .flatten()
            .collect();
        assert!(
            entries.is_empty(),
            "registry/sessions dir must be untouched, found: {entries:?}"
        );

        // No pid-keyed row (*.json) and no name-claim (*.claim) ANYWHERE.
        let mut files = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        walk(root.path(), &mut files);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".json")),
            "no pid-keyed registry row written: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with(".claim")),
            "no name-claim written: {names:?}"
        );
        // Positively: exactly the observe artifacts exist under state/.
        assert!(state_dir.join("marks.jsonl").exists());
        assert!(state_dir
            .join("observed-claims")
            .join("qrmoh~claude~sid-reg")
            .exists());
    }
}
