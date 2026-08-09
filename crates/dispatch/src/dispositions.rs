//! The IMPURE IO layer over the pure [`quorum_dispositions`] leaf crate (qd–qf
//! transition W2, reworked to the R8 event model).
//!
//! The leaf crate owns the wire shapes, the byte-exact serializers, the torn-
//! tail parsers, and the pure fold [`project_summary`]. This module is the fs
//! half: the append writers ([`append_envelope`] / [`append_event`]), the torn-
//! tolerant scoped readers ([`read_scoped`]), the projection query surface
//! ([`query_summary`], the `qd dispositions` DEFAULT) and the raw event read
//! ([`read_events`], the `--events` mode), the inbound-mode idempotency probe
//! ([`has_delivered_event`]), and the v1 [`local_host`] resolver.
//!
//! `dispositions.jsonl` is an **append-only log of typed EVENTS**
//! ([`DispositionEvent`], R8/R8a/R8b + R14) — never state records. Event rows
//! are FULLY NORMALIZED (R14.2, invariant N13): `{v, correlation_id, event,
//! created_at}` + a `class` tail on `delivery-failed`/`refused` — NO `witness`,
//! NO copied `origin`, NO copied `authored_at`. State is a VIEW ([`SummaryRecord`],
//! folded by the leaf's [`project_summary`]); idempotence keys on a `delivered`
//! event EXISTING ([`has_delivered_event`]), never on "any terminal".
//!
//! # House seams (mirrors [`crate::events`] / [`crate::jsonl`])
//!
//! - Filesystem access is std fs against injected **root paths** — this module
//!   takes a resolved [`QdPaths`] and reads/writes the paths it hands out
//!   ([`QdPaths::log_path`] etc.). NOTHING here resolves the real home or reads
//!   `QD_HOME`/hostname directly (lesson L9a): [`local_host`] takes the
//!   injected [`Env`] seam; every path is `QdPaths`-derived.
//! - Errors surface as [`io::Result`] — the append writers do NOT swallow errors.
//!   The CALLER decides fatality (W3 hard-fails a failed log append; best-effort-
//!   warns a failed event append). Readers are best-effort (a missing file
//!   is an empty read, never an error) matching [`crate::events::read_merged`].
//!
//! # Single-writer law + the flock guard (format doc "common framing")
//!
//! qd (the program) is the sole writer of `log.jsonl` / `dispositions.jsonl`,
//! forever. But two concurrent `qd send` PROCESSES may append at the same
//! instant, and a `log.jsonl` body can exceed `PIPE_BUF` (4096 B on Linux/macOS)
//! — large prose. A bare `O_APPEND` single `write_all` is atomic ONLY up to
//! `PIPE_BUF`; past it the kernel may interleave two writers' bytes and corrupt
//! the file. So the append is guarded by an EXCLUSIVE advisory lock
//! (`flock(LOCK_EX)`) held across the (implicit `O_APPEND` seek-to-end +) write
//! and released on Drop. This serializes qd's own concurrent invocations —
//! upholding the single-writer law at the byte level regardless of line size.
//! The lock is a sidecar file (`<file>.lock`), so the truth file's bytes are
//! never anything but records; the fd is `O_CLOEXEC` so a caller wrapping an
//! append around a spawn can never leak the lock into a child (the livelock.rs
//! P4 keystone discipline). Precedent: telemetry.rs `acquire_observed_lock`,
//! resume_daemon.rs `acquire_resume_claim`, livelock.rs.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::effects::Env;
use crate::paths::QdPaths;

pub use quorum_dispositions::{
    has_delivered, parse_dispositions, parse_log, project_one, project_summary,
    DispositionEvent, Envelope, EventKind, ReadResult, SummaryRecord, SummaryState,
};

// ===========================================================================
// flock guard
// ===========================================================================

/// RAII exclusive-append lock. Holds the open lock-file fd; `Drop` closes it →
/// the OS releases the `flock(LOCK_EX)` advisory lock (also on process death).
///
/// The lock file is a sidecar (`<target>.lock`), NOT the truth file — so the
/// truth file only ever contains complete records, and the lock's own
/// create/open never perturbs it.
struct AppendLock {
    _file: std::fs::File,
}

impl AppendLock {
    /// Acquire the exclusive append lock for `target` (BLOCKING `LOCK_EX`).
    ///
    /// Blocking (not `LOCK_NB`) is deliberate: a second concurrent `qd send`
    /// must SERIALIZE behind the first, not fail — the whole point is that both
    /// appends land intact. The lock file is created mode-0600 `O_CLOEXEC`
    /// alongside the target (its parent dir is created by the caller before
    /// this runs). A `flock` error surfaces as `io::Error` — the caller then
    /// fails the append (the append is only as durable as the lock).
    fn acquire(target: &Path) -> io::Result<Self> {
        let lock_path = lock_path_for(target);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&lock_path)?;
        // SAFETY: flock on a valid owned fd. LOCK_EX blocks until the lock is
        // ours (a crashed holder is OS-released, so this cannot wedge forever on
        // a dead peer); the fd stays open for the guard's lifetime.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AppendLock { _file: file })
    }
}

/// The sidecar lock path for a truth file: `<file>.lock` (e.g.
/// `log.jsonl.lock`). One lock per truth file; the log and dispositions files
/// have independent locks (they are appended independently).
fn lock_path_for(target: &Path) -> std::path::PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    target.with_file_name(name)
}

// ===========================================================================
// Per-correlation_id CLAIM LOCK (R15 — spans check→deliver→stamp)
// ===========================================================================

/// RAII per-`correlation_id` claim lock (R15). Held across the whole
/// check→deliver→stamp critical section of ONE send so concurrent presentations
/// of the SAME id SERIALIZE: the winner reads the ledger, delivers, and stamps
/// its `delivered` (with `body_digest`); the loser then acquires the lock, re-
/// reads the now-updated ledger, and resolves against the winner's fact (no-op on
/// an identical body, `refused{body-mismatch}` on a different one). This closes
/// the check-then-act double-delivery race (security audit #1) AND serializes the
/// R15 body-consistency check. `Drop` releases the advisory lock (also on process
/// death — a crashed holder never wedges a peer).
///
/// # Lock ORDERING (deadlock-freedom)
///
/// The claim lock is the OUTERMOST lock: callers acquire it BEFORE any
/// dispositions/log [`AppendLock`] (the stamp inside the critical section takes
/// the append-lock — that is fine, claim is strictly outer). NEVER acquire an
/// append-lock and then a claim-lock; consistent ordering keeps the two lock
/// classes deadlock-free. The lock is PER-`correlation_id`, so only same-id
/// presentations serialize — different ids stay fully concurrent — and the hold
/// is bounded by the delivery's own timeout (incl. the wake).
///
/// # Claim-file lifecycle (v1)
///
/// One tiny empty lock file accumulates per id ever sent, under a `claims/`
/// subdir (kept out of the dispositions dir root so it never clutters the
/// transport files). Accumulation is accepted for v1 (the store is born empty and
/// the files are 0 bytes); a periodic `claims/` gc sweep is a documented
/// FOLLOW-UP. We deliberately do NOT unlink a claim file — unlinking under flock
/// is racy (a waiter may already hold the fd for that path), so the safe policy is
/// leave-in-place + sweep-later.
pub struct ClaimLock {
    _file: std::fs::File,
}

/// The claim-lock sidecar path for `correlation_id`:
/// `<dispatch_root>/claims/<hex-sha256(cid)>.lock`. The cid is HASHED into the
/// filename (not used raw) so an arbitrary caller-supplied `--correlation-id`
/// (frame's ledger event id — any bytes) can never traverse the path, exceed the
/// filename length limit, or collide with a transport file; the hash is a
/// fixed-width, path-safe, collision-resistant function of the id. The files live
/// under a `claims/` subdir so they never clutter the dispositions dir root.
fn claim_lock_path(paths: &QdPaths, correlation_id: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(correlation_id.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    paths.dispatch_root.join("claims").join(format!("{hex}.lock"))
}

/// Acquire the exclusive per-`correlation_id` claim lock (BLOCKING `LOCK_EX`),
/// creating the `claims/` subdir + the sidecar lock file if absent. Blocking (not
/// `LOCK_NB`) is deliberate: a second concurrent presentation of the same id must
/// SERIALIZE behind the first, not fail — the whole point is that they resolve in
/// order against one another's stamped facts. Mode-0600, `O_CLOEXEC` (so a send
/// that wraps a spawn — a wake — can never leak the lock into a child). A `flock`
/// error surfaces as `io::Error`; the caller then fails closed rather than
/// proceeding unserialized. MUST be acquired OUTERMOST — before any append-lock
/// (see [`ClaimLock`] lock-ordering).
pub fn acquire_claim(paths: &QdPaths, correlation_id: &str) -> io::Result<ClaimLock> {
    let lock_path = claim_lock_path(paths, correlation_id);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&lock_path)?;
    // SAFETY: flock on a valid owned fd. LOCK_EX blocks until the lock is ours;
    // an OS-released lock on a dead holder means this cannot wedge on a peer's
    // crash. The fd stays open for the guard's lifetime.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ClaimLock { _file: file })
}

// ===========================================================================
// Append writers (write-then-deliver durability)
// ===========================================================================

/// Append `env.to_jsonl_line() + "\n"` to `paths.log_path()` (format doc §1),
/// creating the parent dir + file if absent, mode 0600. ONE `write_all` of the
/// COMPLETE line, guarded by the exclusive [`AppendLock`] so a concurrent
/// appender's large body can never interleave (see the module doc).
///
/// Returns [`io::Result`]: the CALLER decides fatality (W3 hard-fails a failed
/// log append). Errors are NOT swallowed here.
pub fn append_envelope(paths: &QdPaths, env: &Envelope) -> io::Result<()> {
    append_line(&paths.log_path(), &env.to_jsonl_line())
}

/// Append `event.to_jsonl_line() + "\n"` to `paths.dispositions_path()` (format
/// doc §2 — one typed EVENT row, normalized `{v, correlation_id, event,
/// created_at, [class]}`, R14.2), same durability contract as
/// [`append_envelope`]. The CALLER decides fatality (the stamp points
/// best-effort-warn a failed event append — a lost event row never changes a
/// send's exit).
pub fn append_event(paths: &QdPaths, event: &DispositionEvent) -> io::Result<()> {
    append_line(&paths.dispositions_path(), &event.to_jsonl_line())
}

/// The shared append core: create the parent dir, take the exclusive append
/// lock, then do ONE `O_APPEND | O_CREAT` mode-0600 `write_all` of the COMPLETE
/// record (`line` + `"\n"` as a single buffer — audit #3: the `\n` shares the
/// record's one write, closing the two-write window where a COMPLETE record
/// persisted un-terminated and fused the next append; see the body for the
/// residual mid-write-crash case).
///
/// The lock is held across the append (acquired before the truth-file open,
/// dropped after the write) so two writers serialize at the byte level even for
/// a line larger than `PIPE_BUF` (module doc). `O_APPEND` re-seeks to EOF on the
/// write; combined with the exclusive lock, the record lands whole.
fn append_line(target: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Hold the exclusive lock across the whole open+write critical section.
    let _lock = AppendLock::acquire(target)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(target)?;
    // ONE `write_all` of the COMPLETE record — the line AND its terminating '\n'
    // as a SINGLE buffer (audit #3 fix). A prior version wrote the bytes and the
    // '\n' in two SEPARATE `write_all` calls; a crash / ENOSPC / EINTR BETWEEN the
    // two successful writes would leave a COMPLETE line on disk WITHOUT its
    // terminator, and the next append would fuse onto it — the torn-tail rule then
    // silently drops BOTH records (the reader treats the un-terminated
    // concatenation as one torn tail) while the appends returned exit 0. Building
    // `line + "\n"` up front removes that gap: the terminator is never a separate
    // syscall from the record, so there is no point BETWEEN two successful writes
    // where a COMPLETE line sits un-terminated. (A crash MID-write can still
    // truncate the buffer before its last byte, leaving a partial that a later
    // append would fuse — but that residual needs fsync / atomic-framing, out of
    // scope; what the single write removes is the two-write window where a whole
    // record sat un-terminated.) A COMPLETED `write_all` always lands the whole
    // record incl. its newline. Under the flock this is the single-writer,
    // whole-record append the format contract requires.
    let mut record = String::with_capacity(line.len() + 1);
    record.push_str(line);
    record.push('\n');
    f.write_all(record.as_bytes())
    // `_lock` drops here → the OS releases the advisory lock.
}

// ===========================================================================
// Readers (torn-tail tolerant via the leaf crate)
// ===========================================================================

/// Read `paths.log_path()` and parse it into [`Envelope`] rows (torn-tail
/// tolerant, `v == 1` enforced — the leaf crate's [`parse_log`]). A MISSING file
/// is an empty [`ReadResult`], never an error (best-effort read, matching
/// [`crate::events::read_merged`]).
pub fn read_local_log(paths: &QdPaths) -> ReadResult<Envelope> {
    parse_log(&read_bytes_or_empty(&paths.log_path()))
}

/// Read `paths.dispositions_path()` into [`DispositionEvent`] rows (torn-tail
/// tolerant via [`parse_dispositions`], which also enforces the discriminated-
/// union invariants — the required `class` tail on delivery-failed/refused, no
/// foreign fields, R14.5). MISSING ⇒ empty [`ReadResult`].
pub fn read_local_events(paths: &QdPaths) -> ReadResult<DispositionEvent> {
    parse_dispositions(&read_bytes_or_empty(&paths.dispositions_path()))
}

/// Read a file's bytes, or an EMPTY vec if it is absent/unreadable. Best-effort:
/// a missing transport file is the born-empty state (format doc migration note),
/// never an error — exactly the posture [`crate::events::read_merged`] takes.
fn read_bytes_or_empty(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

/// The read SCOPE for [`read_scoped`] / [`query_summary`] / [`read_events`]
/// (TRANSITION §: `--host`/`--all` union in the remote replicas). Provenance is
/// the CONTAINER a row lives in (a local file ⇒ this host, `remote/<host>/` ⇒
/// that host) — event rows carry NO `witness`/`origin` column (R14.2), so the
/// union order itself must be deterministic (see [`read_scoped`]'s sorted-host
/// concatenation, R14a pin 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Local hot files only (`log.jsonl` + `dispositions.jsonl`). Default scope.
    Local,
    /// Local UNION one peer's `remote/<host>/` replicas (`--host <h>`).
    Host(String),
    /// Local UNION every `remote/<host>/` replica (`--all`).
    All,
}

/// Read the envelopes + disposition EVENTS in `scope`, torn-tail tolerant per
/// file, concatenated (dedup is [`project_summary`]'s job — NOT the reader's).
///
/// - [`Scope::Local`]  = local `log.jsonl` + `dispositions.jsonl`.
/// - [`Scope::Host`]   = local UNION `remote/<h>/{log,dispositions}.jsonl` (the
///   spec: `--host` unions IN the remote replica).
/// - [`Scope::All`]    = local UNION every `remote/<host>/` (enumerate the
///   subdirs of [`QdPaths::remote_dir`]); a MISSING `remote/` ⇒ just local.
/// - `archive = true` additionally unions the LOCAL archive tier
///   (`log.archive.jsonl` + `dispositions.archive.jsonl`); remote has NO archive
///   siblings in the layout. Missing archive files ⇒ skipped.
///
/// # Cross-source determinism (R14.2 / R14a pin 2)
///
/// Event rows no longer carry a `source`/`witness` column (normalized away,
/// R14.2), so the leaf fold is `(created_at, input-order)` — it relies on THIS
/// layer to hand it a slice whose input-order encodes source order. So the
/// concatenation is a FIXED, SORTED-HOST order: the local hot tier FIRST, then
/// the local archive tier, then the remote hosts in SORTED order ([`remote_hosts`]
/// sorts; [`Scope::Host`] is a single host). This makes the flat `Vec` — and thus
/// the projection's same-`created_at` tie-break — INVARIANT under any filesystem
/// scan / directory-enumeration order (the exact nondeterminism R11.2 pinned;
/// proven by `read_scoped_cross_source_determinism` below).
///
/// Torn-tail / interior corruption is tolerated per-file (leaf-crate parsers). A
/// nonzero `corrupt_interior` on any file is logged as a best-effort warn (never
/// fatal, never a verdict input). Returns `io::Result` only because enumerating
/// `remote/` can surface a real IO error other than not-found; a not-found
/// `remote/` is NOT an error.
pub fn read_scoped(
    paths: &QdPaths,
    scope: &Scope,
    archive: bool,
) -> io::Result<(Vec<Envelope>, Vec<DispositionEvent>)> {
    let mut envelopes: Vec<Envelope> = Vec::new();
    let mut events: Vec<DispositionEvent> = Vec::new();

    // Local hot tier (always in scope).
    accumulate(
        &paths.log_path(),
        &paths.dispositions_path(),
        &mut envelopes,
        &mut events,
    );

    // Local archive tier (LOCAL only — remote has no archive siblings).
    if archive {
        accumulate(
            &paths.log_archive_path(),
            &paths.dispositions_archive_path(),
            &mut envelopes,
            &mut events,
        );
    }

    // Remote replicas per scope.
    match scope {
        Scope::Local => {}
        Scope::Host(h) => {
            accumulate(
                &paths.remote_log_path(h),
                &paths.remote_dispositions_path(h),
                &mut envelopes,
                &mut events,
            );
        }
        Scope::All => {
            for host in remote_hosts(paths)? {
                accumulate(
                    &paths.remote_log_path(&host),
                    &paths.remote_dispositions_path(&host),
                    &mut envelopes,
                    &mut events,
                );
            }
        }
    }

    Ok((envelopes, events))
}

/// Read one (log, dispositions) file pair, parse torn-tolerant, and append the
/// records to the running accumulators. Missing files contribute nothing (the
/// best-effort read posture). A nonzero interior-corruption count warns (never
/// fatal, never a verdict input — mirrors the events reader's forensic count).
fn accumulate(
    log_path: &Path,
    disp_path: &Path,
    envelopes: &mut Vec<Envelope>,
    events: &mut Vec<DispositionEvent>,
) {
    let log = parse_log(&read_bytes_or_empty(log_path));
    if log.corrupt_interior > 0 {
        eprintln!(
            "qd: {} — {} corrupt interior line(s) skipped",
            log_path.display(),
            log.corrupt_interior
        );
    }
    envelopes.extend(log.records);

    let disp = parse_dispositions(&read_bytes_or_empty(disp_path));
    if disp.corrupt_interior > 0 {
        eprintln!(
            "qd: {} — {} corrupt interior line(s) skipped",
            disp_path.display(),
            disp.corrupt_interior
        );
    }
    events.extend(disp.records);
}

/// Enumerate the host subdirs under [`QdPaths::remote_dir`] for [`Scope::All`].
/// A MISSING `remote/` ⇒ empty (just local, no error). Any OTHER read error on
/// an existing `remote/` propagates. Only DIRECTORY entries are hosts; stray
/// files are ignored.
fn remote_hosts(paths: &QdPaths) -> io::Result<Vec<String>> {
    let dir = paths.remote_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut hosts = Vec::new();
    for entry in entries {
        let entry = entry?;
        // A directory (or a symlink to one) is a host; skip plain files.
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                hosts.push(name.to_string());
            }
        }
    }
    // Deterministic order for a stable projection scan (project_summary dedups,
    // but a stable input order keeps output order reproducible).
    hosts.sort();
    Ok(hosts)
}

// ===========================================================================
// Query surfaces (what the `qd dispositions` verb calls)
// ===========================================================================

/// THE default read surface for `qd dispositions` (§3a): read the records in
/// `scope` (+ `archive`), then fold them into the published per-id
/// [`SummaryRecord`] rows at `now_ms` via the leaf's [`project_summary`]. If
/// `only` is `Some(id)`, filter to that one `correlation_id` ([`project_one`]
/// semantics — at most one record per id).
///
/// This is the exact view the format doc §3 publishes: envelopes ∪ events →
/// one summary per id, `pending`/`expired` derived from ABSENCE relative to the
/// envelope's own `expires_at` at `now_ms`, the coarse state ruled by the
/// RATIFIED R10 precedence (delivered > expired > failed > pending). `now_ms`
/// is the caller's clock reading (the verb passes an injected
/// [`crate::effects::Clock`]) — this fn stays pure over it.
pub fn query_summary(
    paths: &QdPaths,
    scope: &Scope,
    archive: bool,
    now_ms: i64,
    only: Option<&str>,
) -> io::Result<Vec<SummaryRecord>> {
    let (envelopes, events) = read_scoped(paths, scope, archive)?;
    let records = match only {
        Some(id) => project_one(&envelopes, &events, now_ms, id)
            .into_iter()
            .collect(),
        None => project_summary(&envelopes, &events, now_ms),
    };
    Ok(records)
}

/// The RAW event read for `qd dispositions --events` (§3b): the
/// [`DispositionEvent`] rows in `scope` (+ `archive`), in file/union order —
/// the witnessed funnel itself, no fold, no state. If `only` is `Some(id)`,
/// keep only that `correlation_id`'s rows (still in order).
pub fn read_events(
    paths: &QdPaths,
    scope: &Scope,
    archive: bool,
    only: Option<&str>,
) -> io::Result<Vec<DispositionEvent>> {
    let (_envelopes, mut events) = read_scoped(paths, scope, archive)?;
    if let Some(id) = only {
        events.retain(|e| e.correlation_id() == id);
    }
    Ok(events)
}

// ===========================================================================
// Idempotency helper (W4 inbound mode)
// ===========================================================================

/// True iff the LOCAL `dispositions.jsonl` already carries a `delivered` EVENT
/// for `correlation_id` (the leaf's [`has_delivered`] over
/// [`read_local_events`]). Inbound mode (W4) uses this as the idempotency
/// probe: a delivered event present ⇒ no-op success (the prose already landed;
/// delivery is irreversible, §2).
///
/// Idempotence keys on a delivered event EXISTING (the R8 bug fix) — a
/// `delivery-failed` row does NOT block a retry: a failed attempt is history,
/// not a verdict, and the next presentation of the same envelope attempts
/// delivery again.
///
/// Local-only by design: idempotency keys on THIS qd's witnessed facts (a peer's
/// mirror is not my authority). Torn-tolerant read; a missing file ⇒ `false`.
pub fn has_delivered_event(paths: &QdPaths, correlation_id: &str) -> io::Result<bool> {
    let events = read_local_events(paths);
    Ok(has_delivered(&events.records, correlation_id))
}

/// The LOCAL `log.jsonl` envelope this qd ORIGINATED for `correlation_id`, or
/// `None` if the id is not in its own log (R15 origin-mode no-double-append). The
/// origin door reads this back under the claim lock: a duplicate origin submit
/// (same `--correlation-id` resubmitted) must NOT double-append the envelope —
/// the caller is retrying the SAME authored act. If more than one envelope row
/// somehow shares the id (origin appends once per id, so this is a corruption or
/// a pre-R15 double-append), the FIRST in file order wins — the original act of
/// authorship. Torn-tolerant read; a missing file ⇒ `None`. Local-only (my log is
/// for envelopes I originated; a peer's envelope lives in the mirror).
///
/// COST (v1, noted not optimized): this scans the whole `log.jsonl` — O(log size)
/// per origin send. Fine for the born-empty transition store; an index is a
/// FOLLOW-UP if the log ever grows large.
pub fn logged_envelope(paths: &QdPaths, correlation_id: &str) -> Option<Envelope> {
    read_local_log(paths)
        .records
        .into_iter()
        .find(|e| e.correlation_id == correlation_id)
}

/// The `body_digest` recorded on the LOCAL `delivered` event for
/// `correlation_id`, or `None` if this qd has no delivered event for it (R15).
/// The door uses this under the claim lock to detect a same-id/different-body
/// presentation: a delivered event present with a DIFFERENT digest than the
/// presented body is `refused{body-mismatch}`.
///
/// Local-only (same authority rule as [`has_delivered_event`]). If more than one
/// delivered event somehow exists for the id (delivery is irreversible, so the
/// door never stamps a second — but a corrupt/hand-edited file could), the FIRST
/// in file order wins: it is the authoritative landed fact, and any later one is
/// exactly the tampering this check exists to surface. Torn-tolerant read; a
/// missing file ⇒ `None`.
pub fn recorded_delivered_digest(
    paths: &QdPaths,
    correlation_id: &str,
) -> io::Result<Option<String>> {
    let events = read_local_events(paths);
    Ok(events
        .records
        .iter()
        .find(|e| e.kind() == EventKind::Delivered && e.correlation_id() == correlation_id)
        .and_then(|e| e.body_digest())
        .map(|d| d.to_string()))
}

// ===========================================================================
// Local host resolver (v1 placeholder — host-identity deferred)
// ===========================================================================

/// The host id for THIS qd. The value stamps an [`Envelope`]'s `origin` when this
/// qd ORIGINATES a message (the single normalized HOME of `origin`, R14.2). Event
/// rows no longer carry `origin`/`witness` (normalized away, R14.2 — provenance
/// is the container the row lives in), so this host id rides ONLY on the envelope
/// now; the disposition events join to it by `correlation_id`.
///
/// **v1 placeholder (host-identity is DEFERRED).** Resolution order:
///   1. `QD_HOST` env override (via the injected [`Env`] seam) if set + nonempty;
///   2. else the literal `"local"`.
///
/// There is intentionally NO OS-hostname read here: the crate has no hostname
/// seam, and reading the real machine identity outside the injected seam would
/// violate L9a (the same discipline that forbids raw `QD_HOME`/HOME reads). On a
/// single machine the host id need only disambiguate rows when a peer's log is
/// unioned from `remote/<host>/`; `"local"` suffices until `host-identity.md`
/// lands and defines the real host id (which will replace this fn's fallback).
/// `QD_HOST` is honored now so a multi-host test/dev setup can distinguish
/// hosts without waiting for that work.
pub fn local_host(env: &dyn Env) -> String {
    env.var("QD_HOST")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::path::PathBuf;
    use std::sync::Arc;

    // ---- fixtures -----------------------------------------------------------

    /// A `QdPaths` rooted at a fresh tempdir (QD_HOME unset default layout). The
    /// tempdir is returned so it outlives the paths (drop removes it).
    fn jailed_paths() -> (tempfile::TempDir, QdPaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = QdPaths::from_home(tmp.path());
        (tmp, paths)
    }

    fn env(id: &str, authored: i64, expires: i64) -> Envelope {
        Envelope {
            v: 1,
            correlation_id: id.to_string(),
            authored_at: authored,
            expires_at: expires,
            target: "alpha@brano".to_string(),
            origin: "brano".to_string(),
            body: "hello".to_string(),
        }
    }

    fn attempted(id: &str, created_at: i64) -> DispositionEvent {
        DispositionEvent::attempted(id.to_string(), created_at)
    }

    fn delivered(id: &str, created_at: i64) -> DispositionEvent {
        // A stable per-id test digest (R15). Real deliveries carry the sha-256 of
        // the body; here a fixed token per id suffices for the store tests.
        DispositionEvent::delivered(id.to_string(), created_at, format!("digest-of-{id}"))
    }

    fn failed(id: &str, created_at: i64, class: &str) -> DispositionEvent {
        DispositionEvent::delivery_failed(id.to_string(), created_at, class.to_string())
    }

    fn refused(id: &str, created_at: i64, class: &str) -> DispositionEvent {
        DispositionEvent::refused(id.to_string(), created_at, class.to_string())
    }

    // ---- append then read round-trips --------------------------------------

    #[test]
    fn append_then_read_envelope_round_trip() {
        let (_tmp, paths) = jailed_paths();
        let e = env("01ABC", 10, 1000);
        append_envelope(&paths, &e).unwrap();
        let r = read_local_log(&paths);
        assert_eq!(r.corrupt_interior, 0);
        assert_eq!(r.records, vec![e]);
    }

    #[test]
    fn append_then_read_event_round_trip() {
        let (_tmp, paths) = jailed_paths();
        let ev = failed("01ABC", 500, "wake");
        append_event(&paths, &ev).unwrap();
        let r = read_local_events(&paths);
        assert_eq!(r.corrupt_interior, 0);
        assert_eq!(r.records, vec![ev]);
    }

    #[test]
    fn append_events_preserve_file_order() {
        // The funnel is an ORDERED log — append order == file order == read order.
        let (_tmp, paths) = jailed_paths();
        append_event(&paths, &attempted("a", 1)).unwrap();
        append_event(&paths, &failed("a", 2, "delivery")).unwrap();
        append_event(&paths, &attempted("a", 3)).unwrap();
        append_event(&paths, &delivered("a", 4)).unwrap();
        let r = read_local_events(&paths);
        let kinds: Vec<EventKind> = r.records.iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::Attempted,
                EventKind::DeliveryFailed,
                EventKind::Attempted,
                EventKind::Delivered
            ]
        );
    }

    #[test]
    fn append_creates_parent_dirs_and_appends_in_order() {
        let (_tmp, paths) = jailed_paths();
        // The dispatch_root dir does not exist yet — append must create it.
        assert!(!paths.dispatch_root.exists());
        append_envelope(&paths, &env("a", 1, 2)).unwrap();
        append_envelope(&paths, &env("b", 3, 4)).unwrap();
        let r = read_local_log(&paths);
        let ids: Vec<&str> = r.records.iter().map(|e| e.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "append order == file order");
    }

    #[test]
    fn missing_files_read_empty_not_error() {
        let (_tmp, paths) = jailed_paths();
        assert!(read_local_log(&paths).records.is_empty());
        assert!(read_local_events(&paths).records.is_empty());
        // read_scoped over a bare root: empty, no error.
        let (envs, events) = read_scoped(&paths, &Scope::Local, false).unwrap();
        assert!(envs.is_empty() && events.is_empty());
    }

    #[test]
    fn append_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("a", 1, 2)).unwrap();
        let mode = std::fs::metadata(paths.log_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "truth file is mode 0600");
    }

    // ---- audit #3: torn/fused append (single write_all of the whole record) ----

    #[test]
    fn append_always_leaves_a_newline_terminated_file() {
        // THE anti-fusion invariant: after EVERY append the on-disk file ends with
        // '\n', so the NEXT append can never fuse onto an un-terminated tail. This
        // is what the single-`write_all`-of-`line + "\n"` guarantees — the audit #3
        // fix ensures the terminator is never a separate syscall that a crash /
        // ENOSPC / EINTR could skip. A large (>PIPE_BUF) body — the case a short
        // write is most plausible on — is included: `write_all` loops over short
        // writes, but the newline is part of the one buffer, so a completed
        // `write_all` never lands a record without its terminator.
        let (_tmp, paths) = jailed_paths();
        let big = "x".repeat(9000); // comfortably > PIPE_BUF (4096)
        let bodies = ["a", "", "line with spaces", &big, "後"];
        for (i, body) in bodies.iter().enumerate() {
            let e = Envelope {
                v: 1,
                correlation_id: format!("id{i}"),
                authored_at: 1,
                expires_at: 2,
                target: "t".to_string(),
                origin: "o".to_string(),
                body: body.to_string(),
            };
            append_envelope(&paths, &e).unwrap();
            // The whole file ends with exactly one '\n' after each append.
            let bytes = std::fs::read(paths.log_path()).unwrap();
            assert_eq!(
                bytes.last(),
                Some(&b'\n'),
                "after append {i} the file is newline-terminated (no un-terminated tail to fuse onto)"
            );
        }
        // And every record parsed — none fused or lost (0 corrupt interior).
        let r = read_local_log(&paths);
        assert_eq!(r.corrupt_interior, 0, "no fused/lost records");
        assert_eq!(r.records.len(), bodies.len(), "every appended record present");
    }

    #[test]
    fn append_line_does_exactly_one_write_all() {
        // MUTATION GUARD (audit #3): the fix is a SINGLE `write_all` of the
        // combined `line + "\n"` buffer. A revert to the two-write form (bytes,
        // then a separate `b"\n"`) reopens the torn/fused-record hole — where a
        // crash between the two writes leaves a record without its newline. Pin the
        // single-write shape at source so a future edit cannot silently reintroduce
        // the second write. (The behavioural invariant above proves the effect;
        // this proves the mechanism, since a partial-write injection is not
        // exercisable at this layer.)
        let src = include_str!("dispositions.rs");
        let start = src
            .find("fn append_line(")
            .expect("append_line must exist");
        // Scope to the fn body: it ends at the next top-level item boundary.
        let after = &src[start..];
        let end = after
            .find("\n// ===")
            .expect("a section boundary follows append_line");
        let body = &after[..end];
        // Count the actual write CALLS (`f.write_all(`), not the word in comments.
        let write_calls = body.matches("f.write_all(").count();
        assert_eq!(
            write_calls, 1,
            "append_line must do EXACTLY ONE f.write_all(...) (audit #3 anti-fusion); found {write_calls}. Body:\n{body}"
        );
        // And that one write must NOT be the bare `b\"\\n\"` terminator alone — the
        // record's newline lives inside the single buffer (the fix builds it up).
        assert!(
            !body.contains("f.write_all(b\"\\n\")"),
            "the newline must be part of the single record buffer, not a separate write. Body:\n{body}"
        );
    }

    // ---- concurrent-append safety (validates the flock guard) --------------

    #[test]
    fn concurrent_appends_every_line_intact() {
        // Spawn several threads, each appending many DISTINCT large-body lines to
        // the SAME log. Without the flock guard, two >PIPE_BUF bodies could
        // interleave and corrupt the file. Assert: every line parses (0 corrupt)
        // and the exact expected set of ids is present, none lost/duplicated.
        let (tmp, paths) = jailed_paths();
        let paths = Arc::new(paths);
        // Ensure the target dir exists so the first appender is not racing dir
        // creation (append_line create_dir_all is idempotent, but this keeps the
        // test's focus on the write interleave).
        std::fs::create_dir_all(&paths.dispatch_root).unwrap();

        const THREADS: usize = 4;
        const PER_THREAD: usize = 50;
        // A body comfortably larger than PIPE_BUF (4096) so a bare O_APPEND would
        // NOT be atomic — this is exactly what the flock guard must cover.
        let big = "x".repeat(8192);

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let paths = Arc::clone(&paths);
            let big = big.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let id = format!("t{t}-n{i}");
                    let e = Envelope {
                        v: 1,
                        correlation_id: id,
                        authored_at: 1,
                        expires_at: 2,
                        target: "t".to_string(),
                        origin: "o".to_string(),
                        body: big.clone(),
                    };
                    append_envelope(&paths, &e).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let r = read_local_log(&paths);
        assert_eq!(
            r.corrupt_interior, 0,
            "no interleave/corruption under the flock guard"
        );
        assert_eq!(
            r.records.len(),
            THREADS * PER_THREAD,
            "every appended line present exactly once"
        );
        // Exact id set (no loss, no dup).
        let mut got: Vec<String> = r.records.iter().map(|e| e.correlation_id.clone()).collect();
        got.sort();
        let mut want: Vec<String> = (0..THREADS)
            .flat_map(|t| (0..PER_THREAD).map(move |i| format!("t{t}-n{i}")))
            .collect();
        want.sort();
        assert_eq!(got, want);
        drop(tmp);
    }

    // ---- read_scoped unions -------------------------------------------------

    /// Seed a `remote/<host>/` fixture (log + dispositions) by writing raw bytes.
    fn seed_remote(paths: &QdPaths, host: &str, envs: &[Envelope], events: &[DispositionEvent]) {
        let dir = paths.remote_dir().join(host);
        std::fs::create_dir_all(&dir).unwrap();
        let log: String = envs.iter().map(|e| format!("{}\n", e.to_jsonl_line())).collect();
        std::fs::write(paths.remote_log_path(host), log).unwrap();
        let dp: String = events.iter().map(|d| format!("{}\n", d.to_jsonl_line())).collect();
        std::fs::write(paths.remote_dispositions_path(host), dp).unwrap();
    }

    #[test]
    fn scope_local_excludes_remote_host_all_include() {
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("local1", 1, 1000)).unwrap();
        seed_remote(
            &paths,
            "peerbox",
            &[env("remote1", 2, 1000)],
            &[delivered("remote1", 3)],
        );

        // Local: remote NOT included.
        let (envs, _) = read_scoped(&paths, &Scope::Local, false).unwrap();
        let ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["local1"], "Local scope excludes remote");

        // Host(peerbox): local UNION the peer.
        let (envs, events) = read_scoped(&paths, &Scope::Host("peerbox".into()), false).unwrap();
        let mut ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["local1", "remote1"], "Host unions in the peer");
        assert_eq!(events.len(), 1, "peer event unioned in");

        // All: local UNION every remote host.
        let (envs, _) = read_scoped(&paths, &Scope::All, false).unwrap();
        let mut ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["local1", "remote1"], "All unions every peer");
    }

    #[test]
    fn scope_all_multiple_hosts_and_missing_remote() {
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("local1", 1, 1000)).unwrap();

        // No remote/ dir yet → All is just local.
        let (envs, _) = read_scoped(&paths, &Scope::All, false).unwrap();
        assert_eq!(envs.len(), 1, "missing remote/ ⇒ local only");

        // Two hosts → both unioned.
        seed_remote(&paths, "hostA", &[env("a1", 2, 1000)], &[]);
        seed_remote(&paths, "hostB", &[env("b1", 3, 1000)], &[]);
        let (envs, _) = read_scoped(&paths, &Scope::All, false).unwrap();
        let mut ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a1", "b1", "local1"]);
    }

    // ---- R14a pin 2 — cross-source determinism (the relayered R11.2 test) ----

    #[test]
    fn read_scoped_cross_source_determinism() {
        // R14.2 normalized events carry NO `source`/`witness` column, so the leaf
        // fold is (created_at, input-order). Cross-source determinism is THIS
        // layer's job: read_scoped concatenates the per-host event files in
        // SORTED-HOST order (local first, then remote hosts sorted), so the flat
        // Vec — and the projection's same-created_at tie-break — is INVARIANT under
        // ANY filesystem/directory-enumeration order (the exact nondeterminism
        // R11.2 pinned; now a store-layer concern, not the leaf's).
        //
        // Setup: TWO remote hosts each hold ONE event for the same id "shared" at
        // the SAME created_at. hostA says delivery-failed, hostB says attempted. If
        // the concatenation followed raw scan order, `last_event` on the tie could
        // flip between runs. Because the order is sorted-host (hostA before hostB),
        // hostB's `attempted` is always later-in-input and always wins the tie →
        // last_event=Attempted, deterministically.
        let (_tmp, paths) = jailed_paths();
        let t = 500; // the SAME created_at on both hosts (the discriminating tie)
        seed_remote(&paths, "hostA", &[], &[failed("shared", t, "wake")]);
        seed_remote(&paths, "hostB", &[], &[attempted("shared", t)]);

        // Read the union MANY times: the sorted-host concatenation is stable, so
        // both the raw event order AND the folded summary are byte-for-byte equal
        // across every read regardless of the underlying read_dir order.
        let (_e0, ev0) = read_scoped(&paths, &Scope::All, false).unwrap();
        let s0 = project_summary(&[], &ev0, 600);
        for _ in 0..20 {
            let (_e, ev) = read_scoped(&paths, &Scope::All, false).unwrap();
            // The flat event Vec is identical (sorted-host concatenation) …
            assert_eq!(ev, ev0, "read_scoped event order is invariant across reads");
            // … and so is the projection over it.
            assert_eq!(project_summary(&[], &ev, 600), s0, "summary is order-invariant");
        }

        // And the tie resolves to the SORTED-LAST host's event (hostB's attempted),
        // proving the order is sorted-host, not scan order.
        assert_eq!(ev0.len(), 2, "one event from each host");
        assert_eq!(ev0[0].kind(), EventKind::DeliveryFailed, "hostA (sorted first)");
        assert_eq!(ev0[1].kind(), EventKind::Attempted, "hostB (sorted last)");
        assert_eq!(s0.len(), 1);
        assert_eq!(
            s0[0].last_event,
            Some(EventKind::Attempted),
            "the sorted-last host wins the equal-created_at tie, deterministically"
        );
        // An orphan-event summary (no envelope in scope) is triple-null (R14.2).
        assert_eq!(s0[0].origin, None);
        assert_eq!(s0[0].authored_at, None);
        assert_eq!(s0[0].expires_at, None);
    }

    #[test]
    fn archive_flag_unions_local_archive_tier() {
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("hot1", 1, 1000)).unwrap();
        // Seed the LOCAL archive tier directly.
        std::fs::create_dir_all(&paths.dispatch_root).unwrap();
        std::fs::write(
            paths.log_archive_path(),
            format!("{}\n", env("arch1", 2, 1000).to_jsonl_line()),
        )
        .unwrap();

        // archive=false: archive tier NOT read.
        let (envs, _) = read_scoped(&paths, &Scope::Local, false).unwrap();
        let ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["hot1"], "archive tier excluded when archive=false");

        // archive=true: archive tier unioned in.
        let (envs, _) = read_scoped(&paths, &Scope::Local, true).unwrap();
        let mut ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["arch1", "hot1"], "archive tier unioned when archive=true");
    }

    #[test]
    fn read_scoped_tolerates_torn_and_interior_corruption() {
        let (_tmp, paths) = jailed_paths();
        std::fs::create_dir_all(&paths.dispatch_root).unwrap();
        // One good line, one interior-garbage line, one torn tail (no \n).
        let good = env("good", 1, 1000).to_jsonl_line();
        let content = format!("{good}\nnot json\n{{\"v\":1,\"correlation_i");
        std::fs::write(paths.log_path(), content).unwrap();
        // read_scoped must still return the one good record (best-effort).
        let (envs, _) = read_scoped(&paths, &Scope::Local, false).unwrap();
        let ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["good"], "good record survives torn+corrupt siblings");
    }

    // ---- query_summary end-to-end -------------------------------------------

    #[test]
    fn query_summary_projects_delivered_pending_and_filters() {
        let (_tmp, paths) = jailed_paths();
        // envelope "d" attempted + delivered; envelope "p" pending pre-expiry.
        append_envelope(&paths, &env("d", 10, 1000)).unwrap();
        append_envelope(&paths, &env("p", 10, 1000)).unwrap();
        append_event(&paths, &attempted("d", 400)).unwrap();
        append_event(&paths, &delivered("d", 500)).unwrap();

        // now < expires for "p".
        let all = query_summary(&paths, &Scope::Local, false, 42, None).unwrap();
        let by_id = |id: &str| all.iter().find(|r| r.correlation_id == id).cloned();
        let d = by_id("d").unwrap();
        assert_eq!(d.state, SummaryState::Delivered);
        assert_eq!(d.attempts, 1);
        assert_eq!(d.last_event, Some(EventKind::Delivered));
        // R14.2: origin/authored_at come ONLY from the joined envelope now.
        assert_eq!(d.origin.as_deref(), Some("brano"), "from the joined envelope");
        assert_eq!(d.authored_at, Some(10), "from the joined envelope");
        assert_eq!(d.first_delivered_at, Some(500));
        let p = by_id("p").unwrap();
        assert_eq!(p.state, SummaryState::Pending);
        // R11.1: last_event null iff no events. origin/authored_at still come from
        // the envelope (p has an envelope in scope).
        assert_eq!(p.last_event, None);
        assert_eq!(p.origin.as_deref(), Some("brano"), "envelope in scope");
        assert_eq!(p.attempts, 0);

        // only=Some filters to that id (project_one semantics).
        let one = query_summary(&paths, &Scope::Local, false, 42, Some("d")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].correlation_id, "d");
        assert_eq!(one[0].state, SummaryState::Delivered);

        // A miss yields an empty vec (not an error).
        let none = query_summary(&paths, &Scope::Local, false, 42, Some("nope")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn query_summary_pre_expiry_pending_then_expired_post_expiry() {
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("e", 10, 1000)).unwrap();
        // now < expires → pending.
        let pending = query_summary(&paths, &Scope::Local, false, 999, Some("e")).unwrap();
        assert_eq!(pending[0].state, SummaryState::Pending);
        // now >= expires → expired (view-computed, not stored).
        let expired = query_summary(&paths, &Scope::Local, false, 1000, Some("e")).unwrap();
        assert_eq!(expired[0].state, SummaryState::Expired);
        assert_eq!(expired[0].last_event, None);
        // origin/authored_at still from the envelope (in scope), even when expired.
        assert_eq!(expired[0].origin.as_deref(), Some("brano"));
    }

    #[test]
    fn query_summary_folds_fail_then_retry_to_delivered() {
        // The store-level echo of the §6 funnel: a delivery-failed row followed by
        // a successful retry folds to Delivered, attempts=2 — never failed forever.
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("f", 10, 1_000_000)).unwrap();
        append_event(&paths, &attempted("f", 100)).unwrap();
        append_event(&paths, &failed("f", 100, "delivery")).unwrap();
        append_event(&paths, &attempted("f", 200)).unwrap();
        append_event(&paths, &delivered("f", 300)).unwrap();
        let s = query_summary(&paths, &Scope::Local, false, 400, Some("f"))
            .unwrap()
            .remove(0);
        assert_eq!(s.state, SummaryState::Delivered, "delivered event exists");
        assert_eq!(s.attempts, 2);
        assert_eq!(s.last_event, Some(EventKind::Delivered));
        assert_eq!(s.first_delivered_at, Some(300));
        assert_eq!(s.last_attempt_at, Some(200));
    }

    // ---- read_events (the raw --events surface) ------------------------------

    #[test]
    fn read_events_preserves_order_and_filters_to_only() {
        let (_tmp, paths) = jailed_paths();
        append_event(&paths, &attempted("a", 1)).unwrap();
        append_event(&paths, &attempted("b", 2)).unwrap();
        append_event(&paths, &failed("a", 3, "delivery")).unwrap();
        append_event(&paths, &delivered("a", 4)).unwrap();

        // No filter: every row, file order.
        let all = read_events(&paths, &Scope::Local, false, None).unwrap();
        let got: Vec<(String, EventKind)> = all
            .iter()
            .map(|e| (e.correlation_id().to_string(), e.kind()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), EventKind::Attempted),
                ("b".to_string(), EventKind::Attempted),
                ("a".to_string(), EventKind::DeliveryFailed),
                ("a".to_string(), EventKind::Delivered),
            ],
            "raw rows in file order"
        );

        // only=Some(id): that id's rows, still in file order.
        let a_only = read_events(&paths, &Scope::Local, false, Some("a")).unwrap();
        let kinds: Vec<EventKind> = a_only.iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            vec![EventKind::Attempted, EventKind::DeliveryFailed, EventKind::Delivered]
        );
        assert!(a_only.iter().all(|e| e.correlation_id() == "a"));

        // A miss is empty, not an error.
        assert!(read_events(&paths, &Scope::Local, false, Some("zz")).unwrap().is_empty());
    }

    #[test]
    fn read_events_unions_remote_scope() {
        let (_tmp, paths) = jailed_paths();
        append_event(&paths, &attempted("local-id", 1)).unwrap();
        seed_remote(&paths, "peerbox", &[], &[delivered("remote-id", 2)]);
        let all = read_events(&paths, &Scope::All, false, None).unwrap();
        let mut ids: Vec<&str> = all.iter().map(|e| e.correlation_id()).collect();
        ids.sort();
        assert_eq!(ids, vec!["local-id", "remote-id"]);
    }

    #[test]
    fn query_summary_refused_row_is_pending_class() {
        // R14.3: a `refused` event is PENDING-class (refused = never left ≠ failed)
        // and is NOT an attempt. A refused-only id summarizes to pending with
        // last_event=refused, attempts=0.
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("r", 10, 1_000_000)).unwrap();
        append_event(&paths, &refused("r", 60, "no-live-receive-path")).unwrap();
        let s = query_summary(&paths, &Scope::Local, false, 100, Some("r"))
            .unwrap()
            .remove(0);
        assert_eq!(s.state, SummaryState::Pending, "refused ≠ failed, pending-class");
        assert_eq!(s.last_event, Some(EventKind::Refused));
        assert_eq!(s.attempts, 0, "refused is not an attempt");
    }

    // ---- has_delivered_event -------------------------------------------------

    #[test]
    fn has_delivered_event_true_only_when_a_delivered_event_exists() {
        let (_tmp, paths) = jailed_paths();
        assert!(!has_delivered_event(&paths, "x").unwrap(), "no file ⇒ false");
        append_event(&paths, &attempted("x", 4)).unwrap();
        assert!(
            !has_delivered_event(&paths, "x").unwrap(),
            "attempted alone is not delivered"
        );
        append_event(&paths, &delivered("x", 5)).unwrap();
        assert!(has_delivered_event(&paths, "x").unwrap(), "delivered event ⇒ true");
        assert!(!has_delivered_event(&paths, "y").unwrap(), "other id ⇒ false");
    }

    #[test]
    fn has_delivered_event_failed_row_does_not_block_a_retry() {
        // THE discriminator (the R8 bug fix): a delivery-failed row present ⇒
        // has_delivered_event == false — the idempotency probe must NOT treat a
        // failure as terminal, so a retry of the same envelope is not blocked.
        // (The old has_terminal treated ANY row as terminal — that model is dead.)
        let (_tmp, paths) = jailed_paths();
        append_event(&paths, &failed("f", 5, "wake")).unwrap();
        assert!(
            !has_delivered_event(&paths, "f").unwrap(),
            "a delivery-failed row must not block the retry"
        );
        // The retry then delivers — NOW it is idempotent-terminal.
        append_event(&paths, &delivered("f", 6)).unwrap();
        assert!(has_delivered_event(&paths, "f").unwrap());
    }

    // ---- recorded_delivered_digest (R15) ------------------------------------

    #[test]
    fn recorded_delivered_digest_returns_the_delivered_events_digest() {
        let (_tmp, paths) = jailed_paths();
        // No file ⇒ None. Attempted-only ⇒ None (no delivered event).
        assert_eq!(recorded_delivered_digest(&paths, "x").unwrap(), None);
        append_event(&paths, &attempted("x", 1)).unwrap();
        assert_eq!(
            recorded_delivered_digest(&paths, "x").unwrap(),
            None,
            "attempted alone binds no body"
        );
        // A delivered event ⇒ its digest (the test helper stamps `digest-of-<id>`).
        append_event(&paths, &delivered("x", 2)).unwrap();
        assert_eq!(
            recorded_delivered_digest(&paths, "x").unwrap().as_deref(),
            Some("digest-of-x")
        );
        // A different id is independent.
        assert_eq!(recorded_delivered_digest(&paths, "y").unwrap(), None);
    }

    #[test]
    fn recorded_delivered_digest_first_delivered_wins_on_a_tampered_file() {
        // If a corrupt/hand-edited file somehow carries TWO delivered rows for one
        // id (the door never stamps a second), the FIRST — the authoritative
        // landed fact — wins; a later differing one is exactly the tampering the
        // R15 check exists to surface, not to silently adopt.
        let (_tmp, paths) = jailed_paths();
        append_event(
            &paths,
            &DispositionEvent::delivered("dup".into(), 1, "first-digest".into()),
        )
        .unwrap();
        append_event(
            &paths,
            &DispositionEvent::delivered("dup".into(), 2, "second-digest".into()),
        )
        .unwrap();
        assert_eq!(
            recorded_delivered_digest(&paths, "dup").unwrap().as_deref(),
            Some("first-digest"),
            "the first delivered row is authoritative"
        );
    }

    // ---- claim lock (R15) ----------------------------------------------------

    #[test]
    fn claim_lock_path_is_a_hashed_sidecar_under_the_claims_subdir() {
        let (_tmp, paths) = jailed_paths();
        let claims_dir = paths.dispatch_root.join("claims");
        let p = claim_lock_path(&paths, "01ABC");
        assert_eq!(p.parent().unwrap(), claims_dir, "lock files live under claims/");
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".lock") && name.len() == 64 + ".lock".len(), "got {name}");
        // The cid is HASHED (64 hex chars) — a path-unsafe cid can never traverse.
        let evil = claim_lock_path(&paths, "../../etc/passwd");
        assert_eq!(evil.parent().unwrap(), claims_dir, "a slashy cid stays confined to claims/");
        assert_eq!(
            evil.file_name().unwrap().to_string_lossy().len(),
            64 + ".lock".len(),
            "a slashy cid hashes to a fixed-width safe filename"
        );
        // Distinct ids ⇒ distinct lock files; the same id ⇒ the same file.
        assert_ne!(claim_lock_path(&paths, "a"), claim_lock_path(&paths, "b"));
        assert_eq!(claim_lock_path(&paths, "same"), claim_lock_path(&paths, "same"));
    }

    #[test]
    fn claim_lock_serializes_same_id_holders() {
        // Two threads contend for the SAME id's claim lock; the second BLOCKS until
        // the first drops. We prove serialization by having the holder hold briefly
        // and asserting the two critical sections never overlap.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let (tmp, paths) = jailed_paths();
        std::fs::create_dir_all(&paths.dispatch_root).unwrap();
        let paths = Arc::new(paths);
        let in_section = Arc::new(AtomicBool::new(false));
        let overlaps = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let paths = Arc::clone(&paths);
            let in_section = Arc::clone(&in_section);
            let overlaps = Arc::clone(&overlaps);
            handles.push(std::thread::spawn(move || {
                let _lock = acquire_claim(&paths, "contended-id").unwrap();
                // Entering the critical section: if anyone else is already in it,
                // the lock failed to serialize.
                if in_section.swap(true, Ordering::SeqCst) {
                    overlaps.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                in_section.store(false, Ordering::SeqCst);
                // `_lock` drops here → the next waiter proceeds.
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            overlaps.load(Ordering::SeqCst),
            0,
            "the per-id claim lock serialized every holder (no overlap)"
        );
        // A DIFFERENT id's lock is independent (does not block) — acquire both.
        let _a = acquire_claim(&paths, "id-a").unwrap();
        let _b = acquire_claim(&paths, "id-b").unwrap();
        drop(tmp);
    }

    // ---- local_host ----------------------------------------------------------

    #[test]
    fn local_host_qd_host_override_wins() {
        let mut e = MapEnv::default();
        e.vars.insert("QD_HOST".to_string(), "brano".to_string());
        assert_eq!(local_host(&e), "brano");
    }

    #[test]
    fn local_host_falls_back_to_local() {
        // No QD_HOST → the "local" v1 placeholder.
        let e = MapEnv::default();
        assert_eq!(local_host(&e), "local");
    }

    #[test]
    fn local_host_empty_qd_host_falls_back() {
        // QD_HOST="" is treated as unset (nonempty filter).
        let mut e = MapEnv::default();
        e.vars.insert("QD_HOST".to_string(), String::new());
        assert_eq!(local_host(&e), "local");
    }

    // ---- lock path shape ----------------------------------------------------

    #[test]
    fn lock_path_is_sidecar() {
        assert_eq!(
            lock_path_for(Path::new("/x/log.jsonl")),
            PathBuf::from("/x/log.jsonl.lock")
        );
    }
}
