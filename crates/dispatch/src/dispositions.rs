//! The IMPURE IO layer over the pure [`quorum_dispositions`] leaf crate (qd–qf
//! transition W2).
//!
//! The leaf crate owns the wire shapes, the byte-exact serializers, the torn-
//! tail parsers, and the pure left-join [`project`]. This module is the fs half:
//! the append writers ([`append_envelope`] / [`append_disposition`]), the torn-
//! tolerant scoped readers ([`read_scoped`]), the projection query surface
//! ([`query`], what the W5 `qd dispositions` verb calls), the inbound-mode
//! idempotency probe ([`has_terminal`]), and the v1 [`local_authority`] resolver.
//!
//! # House seams (mirrors [`crate::events`] / [`crate::jsonl`])
//!
//! - Filesystem access is std fs against injected **root paths** — this module
//!   takes a resolved [`QdPaths`] and reads/writes the paths it hands out
//!   ([`QdPaths::log_path`] etc.). NOTHING here resolves the real home or reads
//!   `QD_HOME`/hostname directly (lesson L9a): [`local_authority`] takes the
//!   injected [`Env`] seam; every path is `QdPaths`-derived.
//! - Errors surface as [`io::Result`] — the append writers do NOT swallow errors.
//!   The CALLER decides fatality (W3 hard-fails a failed log append; best-effort-
//!   warns a failed disposition append). Readers are best-effort (a missing file
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
    parse_dispositions, parse_log, project, project_one, Disposition, EmittedRecord, Envelope,
    ReadResult, RecordState, StoredState,
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

/// Append `disp.to_jsonl_line() + "\n"` to `paths.dispositions_path()` (format
/// doc §2), same durability contract as [`append_envelope`]. The CALLER decides
/// fatality (W3 best-effort-warns a failed disposition append).
pub fn append_disposition(paths: &QdPaths, disp: &Disposition) -> io::Result<()> {
    append_line(&paths.dispositions_path(), &disp.to_jsonl_line())
}

/// The shared append core: create the parent dir, take the exclusive append
/// lock, then do ONE `O_APPEND | O_CREAT` mode-0600 `write_all` of `line + "\n"`.
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
    // ONE write_all of the complete line + '\n'. Under the flock this is the
    // single-writer append the format contract requires.
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
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

/// Read `paths.dispositions_path()` into [`Disposition`] rows (torn-tail
/// tolerant via [`parse_dispositions`]). MISSING ⇒ empty [`ReadResult`].
pub fn read_local_dispositions(paths: &QdPaths) -> ReadResult<Disposition> {
    parse_dispositions(&read_bytes_or_empty(&paths.dispositions_path()))
}

/// Read a file's bytes, or an EMPTY vec if it is absent/unreadable. Best-effort:
/// a missing transport file is the born-empty state (format doc migration note),
/// never an error — exactly the posture [`crate::events::read_merged`] takes.
fn read_bytes_or_empty(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

/// The read SCOPE for [`read_scoped`] / [`query`] (TRANSITION §: `--host`/`--all`
/// union in the remote replicas; the `authority` column disambiguates origin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Local hot files only (`log.jsonl` + `dispositions.jsonl`). Default scope.
    Local,
    /// Local UNION one peer's `remote/<host>/` replicas (`--host <h>`).
    Host(String),
    /// Local UNION every `remote/<host>/` replica (`--all`).
    All,
}

/// Read the envelopes + dispositions in `scope`, torn-tail tolerant per file,
/// concatenated (dedup is [`project`]'s job — NOT the reader's).
///
/// - [`Scope::Local`]  = local `log.jsonl` + `dispositions.jsonl`.
/// - [`Scope::Host`]   = local UNION `remote/<h>/{log,dispositions}.jsonl` (the
///   spec: `--host` unions IN the remote replica; `authority` disambiguates).
/// - [`Scope::All`]    = local UNION every `remote/<host>/` (enumerate the
///   subdirs of [`QdPaths::remote_dir`]); a MISSING `remote/` ⇒ just local.
/// - `archive = true` additionally unions the LOCAL archive tier
///   (`log.archive.jsonl` + `dispositions.archive.jsonl`); remote has NO archive
///   siblings in the layout. Missing archive files ⇒ skipped.
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
) -> io::Result<(Vec<Envelope>, Vec<Disposition>)> {
    let mut envelopes: Vec<Envelope> = Vec::new();
    let mut dispositions: Vec<Disposition> = Vec::new();

    // Local hot tier (always in scope).
    accumulate(
        &paths.log_path(),
        &paths.dispositions_path(),
        &mut envelopes,
        &mut dispositions,
    );

    // Local archive tier (LOCAL only — remote has no archive siblings).
    if archive {
        accumulate(
            &paths.log_archive_path(),
            &paths.dispositions_archive_path(),
            &mut envelopes,
            &mut dispositions,
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
                &mut dispositions,
            );
        }
        Scope::All => {
            for host in remote_hosts(paths)? {
                accumulate(
                    &paths.remote_log_path(&host),
                    &paths.remote_dispositions_path(&host),
                    &mut envelopes,
                    &mut dispositions,
                );
            }
        }
    }

    Ok((envelopes, dispositions))
}

/// Read one (log, dispositions) file pair, parse torn-tolerant, and append the
/// records to the running accumulators. Missing files contribute nothing (the
/// best-effort read posture). A nonzero interior-corruption count warns (never
/// fatal, never a verdict input — mirrors the events reader's forensic count).
fn accumulate(
    log_path: &Path,
    disp_path: &Path,
    envelopes: &mut Vec<Envelope>,
    dispositions: &mut Vec<Disposition>,
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
    dispositions.extend(disp.records);
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
    // Deterministic order for a stable projection scan (project dedups, but a
    // stable input order keeps output order reproducible).
    hosts.sort();
    Ok(hosts)
}

// ===========================================================================
// Projection query surface (what the W5 verb calls)
// ===========================================================================

/// THE read surface for `qd dispositions` (W5): read the records in `scope`
/// (+ `archive`), then project them into the emitted 4-state records at
/// `now_ms`. If `only` is `Some(id)`, filter to that one `correlation_id`
/// ([`project_one`] semantics — at most one record per id).
///
/// This is the exact join the format doc §3 publishes: envelopes ⟕ dispositions,
/// `pending`/`expired` derived from ABSENCE relative to the envelope's own
/// `expires_at` at `now_ms`. `now_ms` is the caller's clock reading (the verb
/// passes an injected [`crate::effects::Clock`]) — this fn stays pure over it.
pub fn query(
    paths: &QdPaths,
    scope: &Scope,
    archive: bool,
    now_ms: i64,
    only: Option<&str>,
) -> io::Result<Vec<EmittedRecord>> {
    let (envelopes, dispositions) = read_scoped(paths, scope, archive)?;
    let records = match only {
        Some(id) => project_one(&envelopes, &dispositions, now_ms, id)
            .into_iter()
            .collect(),
        None => project(&envelopes, &dispositions, now_ms),
    };
    Ok(records)
}

// ===========================================================================
// Idempotency helper (W4 inbound mode)
// ===========================================================================

/// True iff the LOCAL `dispositions.jsonl` already carries a terminal row for
/// `correlation_id` (any terminal — `delivered` OR `failed`). Inbound mode (W4)
/// uses this as the idempotency probe: a terminal present ⇒ no-op success (the
/// envelope was already witnessed; "first terminal wins", format doc §2).
///
/// Local-only by design: idempotency keys on THIS qd's witnessed facts (a peer's
/// mirror is not my authority). Torn-tolerant read; a missing file ⇒ `false`.
pub fn has_terminal(paths: &QdPaths, correlation_id: &str) -> io::Result<bool> {
    let disp = read_local_dispositions(paths);
    Ok(disp
        .records
        .iter()
        .any(|d| d.correlation_id == correlation_id))
}

// ===========================================================================
// Local authority resolver (v1 placeholder — host-identity deferred)
// ===========================================================================

/// The host id stamped into an envelope's / disposition's `authority` column.
///
/// **v1 placeholder (host-identity is DEFERRED).** Resolution order:
///   1. `QD_HOST` env override (via the injected [`Env`] seam) if set + nonempty;
///   2. else the literal `"local"`.
///
/// There is intentionally NO OS-hostname read here: the crate has no hostname
/// seam, and reading the real machine identity outside the injected seam would
/// violate L9a (the same discipline that forbids raw `QD_HOME`/HOME reads). On a
/// single machine `authority` need only disambiguate ORIGIN when a peer's log is
/// unioned from `remote/<host>/`; `"local"` suffices until `host-identity.md`
/// lands and defines the real host id (which will replace this fn's fallback).
/// `QD_HOST` is honored now so a multi-host test/dev setup can distinguish
/// authorities without waiting for that work.
pub fn local_authority(env: &dyn Env) -> String {
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
            authority: "brano".to_string(),
            body: "hello".to_string(),
        }
    }

    fn disp(id: &str, state: StoredState, witnessed: i64, reason: Option<&str>) -> Disposition {
        Disposition {
            v: 1,
            correlation_id: id.to_string(),
            state,
            authored_at: 100,
            witnessed_at: witnessed,
            authority: "brano".to_string(),
            reason: reason.map(str::to_string),
        }
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
    fn append_then_read_disposition_round_trip() {
        let (_tmp, paths) = jailed_paths();
        let d = disp("01ABC", StoredState::Failed, 500, Some("wake"));
        append_disposition(&paths, &d).unwrap();
        let r = read_local_dispositions(&paths);
        assert_eq!(r.corrupt_interior, 0);
        assert_eq!(r.records, vec![d]);
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
        assert!(read_local_dispositions(&paths).records.is_empty());
        // read_scoped over a bare root: empty, no error.
        let (envs, disps) = read_scoped(&paths, &Scope::Local, false).unwrap();
        assert!(envs.is_empty() && disps.is_empty());
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
                        authority: "a".to_string(),
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
    fn seed_remote(paths: &QdPaths, host: &str, envs: &[Envelope], disps: &[Disposition]) {
        let dir = paths.remote_dir().join(host);
        std::fs::create_dir_all(&dir).unwrap();
        let log: String = envs.iter().map(|e| format!("{}\n", e.to_jsonl_line())).collect();
        std::fs::write(paths.remote_log_path(host), log).unwrap();
        let dp: String = disps.iter().map(|d| format!("{}\n", d.to_jsonl_line())).collect();
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
            &[disp("remote1", StoredState::Delivered, 3, None)],
        );

        // Local: remote NOT included.
        let (envs, _) = read_scoped(&paths, &Scope::Local, false).unwrap();
        let ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["local1"], "Local scope excludes remote");

        // Host(peerbox): local UNION the peer.
        let (envs, disps) = read_scoped(&paths, &Scope::Host("peerbox".into()), false).unwrap();
        let mut ids: Vec<&str> = envs.iter().map(|e| e.correlation_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["local1", "remote1"], "Host unions in the peer");
        assert_eq!(disps.len(), 1, "peer disposition unioned in");

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

    // ---- query end-to-end ---------------------------------------------------

    #[test]
    fn query_projects_delivered_pending_and_filters() {
        let (_tmp, paths) = jailed_paths();
        // envelope "d" delivered; envelope "p" pending pre-expiry.
        append_envelope(&paths, &env("d", 10, 1000)).unwrap();
        append_envelope(&paths, &env("p", 10, 1000)).unwrap();
        append_disposition(&paths, &disp("d", StoredState::Delivered, 500, None)).unwrap();

        // now < expires for "p".
        let all = query(&paths, &Scope::Local, false, 42, None).unwrap();
        let by_id = |id: &str| all.iter().find(|r| r.correlation_id == id).cloned();
        assert_eq!(by_id("d").unwrap().state, RecordState::Delivered);
        assert_eq!(by_id("d").unwrap().witnessed_at, Some(500));
        assert_eq!(by_id("p").unwrap().state, RecordState::Pending);
        assert_eq!(by_id("p").unwrap().witnessed_at, None);

        // only=Some filters to that id (project_one semantics).
        let one = query(&paths, &Scope::Local, false, 42, Some("d")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].correlation_id, "d");
        assert_eq!(one[0].state, RecordState::Delivered);

        // A miss yields an empty vec (not an error).
        let none = query(&paths, &Scope::Local, false, 42, Some("nope")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn query_pre_expiry_pending_then_expired_post_expiry() {
        let (_tmp, paths) = jailed_paths();
        append_envelope(&paths, &env("e", 10, 1000)).unwrap();
        // now < expires → pending.
        let pending = query(&paths, &Scope::Local, false, 999, Some("e")).unwrap();
        assert_eq!(pending[0].state, RecordState::Pending);
        // now >= expires → expired (view-computed, not stored).
        let expired = query(&paths, &Scope::Local, false, 1000, Some("e")).unwrap();
        assert_eq!(expired[0].state, RecordState::Expired);
        assert_eq!(expired[0].witnessed_at, None);
    }

    // ---- has_terminal -------------------------------------------------------

    #[test]
    fn has_terminal_true_when_terminal_present_false_otherwise() {
        let (_tmp, paths) = jailed_paths();
        assert!(!has_terminal(&paths, "x").unwrap(), "no file ⇒ false");
        append_disposition(&paths, &disp("x", StoredState::Delivered, 5, None)).unwrap();
        assert!(has_terminal(&paths, "x").unwrap(), "terminal present ⇒ true");
        assert!(!has_terminal(&paths, "y").unwrap(), "other id ⇒ false");
    }

    #[test]
    fn has_terminal_matches_failed_terminal_too() {
        let (_tmp, paths) = jailed_paths();
        append_disposition(&paths, &disp("f", StoredState::Failed, 5, Some("wake"))).unwrap();
        assert!(has_terminal(&paths, "f").unwrap(), "failed is a terminal");
    }

    // ---- local_authority ----------------------------------------------------

    #[test]
    fn local_authority_qd_host_override_wins() {
        let mut e = MapEnv::default();
        e.vars.insert("QD_HOST".to_string(), "brano".to_string());
        assert_eq!(local_authority(&e), "brano");
    }

    #[test]
    fn local_authority_falls_back_to_local() {
        // No QD_HOST → the "local" v1 placeholder.
        let e = MapEnv::default();
        assert_eq!(local_authority(&e), "local");
    }

    #[test]
    fn local_authority_empty_qd_host_falls_back() {
        // QD_HOST="" is treated as unset (nonempty filter).
        let mut e = MapEnv::default();
        e.vars.insert("QD_HOST".to_string(), String::new());
        assert_eq!(local_authority(&e), "local");
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
