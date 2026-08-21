//! WS-R R3a-Step-1 — per-session flock LIVENESS lock (R1 §1).
//!
//! A per-session *liveness lock file* (`<state_dir>/liveness/<session_id>.lock`)
//! is opened and held open by every live Dispatch-managed process for the
//! duration of its life. The kernel releases the lock on ANY process exit —
//! clean, signal, or SIGKILL — with NO cooperation from the dying process. This
//! gives an O(1) passive liveness signal: a free lock ⇒ the holder is dead.
//!
//! ## Why flock (the fast path)
//! Today liveness is probed via `/proc/<pid>/stat` (`liveness.rs:176`). That is
//! correct but is a per-pid `/proc` read. The flock check is O(1) and does not
//! walk `/proc`: it is the FAST PATH for liveness; the existing `/proc` +
//! `start_ms` path remains the identity verifier (R1 §1, inv 3 ordering: a
//! `probe_dead`->"dead" MUST be re-confirmed via `/proc start_ms` before any
//! destructive act — `reconcile.rs:plan`'s `is_alive` does this, R3a-Step-3).
//!
//! ## CLOEXEC scoping (P4 — the keystone flock-soundness fix)
//! The lock fd defaults to **`FD_CLOEXEC` SET** (close-on-exec). The original
//! "blanket clear CLOEXEC" was too broad: if the agent forks ANY tool subprocess
//! that inherits the lock fd and outlives the agent, `flock` is NOT released on
//! agent death -> `probe_dead` returns LIVE forever -> a dead session is never
//! tombstoned (the exact drift the lock exists to kill). With CLOEXEC SET,
//! ordinary tool/subprocess `exec`s do NOT inherit the lock. Inheritance is
//! WHITELISTED to the managed-wrapper handoff ONLY, via [`LivenessLock::allow_inherit`],
//! which clears `FD_CLOEXEC` just before that one intended exec.
//!
//! Prior art seed: `idstore.rs:452` (`libc::flock(fd, LOCK_EX)` for the id mint).
//! The per-session liveness lock is the same syscall on a distinct per-session
//! path, with `LOCK_NB` so a probe never blocks.

use std::io;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// The per-session liveness lock path: `<state_dir>/liveness/<session_id>.lock`.
/// Keyed on `session_id` (stable across incarnation), not pid (R1 §8). New
/// `paths.rs`-style helper kept here so `livelock` is self-contained.
pub fn liveness_lock_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("liveness")
        .join(format!("{session_id}.lock"))
}

/// A held liveness lock. Holds an open fd whose `flock(LOCK_EX)` is released by
/// the kernel when the LAST referencing process exits. Defaults to `FD_CLOEXEC`
/// SET so ordinary subprocess `exec`s do not inherit it ([`LivenessLock::allow_inherit`]
/// whitelists the managed-wrapper handoff).
#[derive(Debug)]
pub struct LivenessLock {
    _fd: OwnedFd,
    path: PathBuf,
}

impl LivenessLock {
    /// Acquire the per-session liveness lock. Creates `<state_dir>/liveness/` if
    /// absent, opens (or creates) `<session_id>.lock` `O_RDWR|0600`, sets
    /// `FD_CLOEXEC` (so tool subprocesses do not inherit it), then takes
    /// `flock(LOCK_EX|LOCK_NB)`.
    ///
    /// Returns:
    /// - `Ok(Some(lock))` — the lock was free and is now HELD (this process is
    ///   the live holder for `session_id`).
    /// - `Ok(None)` — a LIVE holder already owns the lock (`EWOULDBLOCK`): a
    ///   process for this session is already alive.
    /// - `Err(_)` — an I/O error opening the lock file (mkdir/open failed).
    pub fn acquire(state_dir: &Path, session_id: &str) -> io::Result<Option<LivenessLock>> {
        let path = liveness_lock_path(state_dir, session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let fd = open_lock_fd(&path)?;
        // Default to close-on-exec (P4): ordinary tool subprocesses must NOT
        // inherit the lock and keep a dead session looking alive.
        set_cloexec(&fd, true)?;
        match flock_nb_ex(&fd) {
            FlockResult::Acquired => Ok(Some(LivenessLock { _fd: fd, path })),
            FlockResult::WouldBlock => Ok(None),
            FlockResult::Err(e) => Err(e),
        }
    }

    /// The lock file path (diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// WHITELIST the lock fd for inheritance across the NEXT managed-wrapper
    /// `exec` ONLY (clears `FD_CLOEXEC`). Call this immediately before execing the
    /// session's intended managed wrapper (the PTY/daemon parent that is the
    /// intended lock holder) — NOWHERE else. After the wrapper handoff the
    /// inherited fd keeps the lock held for the wrapper's life; the original
    /// process's drop closing its own copy does NOT free the lock while the
    /// wrapper holds a copy (last-close semantics). P4: do NOT call this before a
    /// plain tool-subprocess exec (that would re-introduce the false-alive).
    pub fn allow_inherit(&self) -> io::Result<()> {
        set_cloexec(&self._fd, false)
    }
}

/// Probe whether the session's holder is DEAD, O(1), without a `/proc` walk.
///
/// Tries to `open(O_RDWR)` + `flock(LOCK_EX|LOCK_NB)` the lock file:
/// - the `flock` SUCCEEDS (the lock was free) -> the holder is DEAD -> returns
///   `true`. The probe drops its fd immediately (so it never itself becomes a
///   false live holder).
/// - `EWOULDBLOCK` -> a live holder owns the lock -> returns `false`.
/// - the lock file is ABSENT (`ENOENT`) -> no holder ever acquired it, OR it was
///   cleaned up -> treated as `false` (NOT dead): absence of the lock file is not
///   positive death evidence (R1 §1 inv 4: the file's presence/absence is not the
///   registry row). Fail-closed — the `/proc start_ms` confirmer (R3a-Step-3) is
///   the authority for a tombstone; `probe_dead` is only the fast-path hint.
/// - any other open error -> `false` (cannot prove death -> fail-closed alive).
///
/// ORDERING (R1 §1 inv 3 / P4.3): a `probe_dead`->true is a fast-path hint ONLY.
/// Between the probe and a reconcile acting, a fresh session could acquire the
/// lock; the caller (`reconcile::plan`'s `is_alive`) MUST re-confirm via
/// `/proc start_ms` before any tombstone. This function never acts; it only
/// reports the cheap lock state.
pub fn probe_dead(state_dir: &Path, session_id: &str) -> bool {
    let path = liveness_lock_path(state_dir, session_id);
    let fd = match open_lock_fd_existing(&path) {
        Ok(Some(fd)) => fd,
        Ok(None) => return false, // ENOENT: no lock file -> not positive death.
        Err(_) => return false,   // ambiguous open error -> fail-closed alive.
    };
    match flock_nb_ex(&fd) {
        // Lock was free -> holder is dead. Drop the fd immediately so the probe
        // does not itself become a (transient) false live holder.
        FlockResult::Acquired => {
            drop(fd);
            true
        }
        FlockResult::WouldBlock => false, // a live holder owns it.
        FlockResult::Err(_) => false,     // ambiguous -> fail-closed alive.
    }
}

/// Boot self-test (P4): verify the `state_dir` filesystem honors `flock(LOCK_EX)`
/// cross-fd. flock cross-process semantics vary on NFS/overlay/9p; a no-op lock
/// would silently make every session look alive. Opens TWO fds on a throwaway
/// probe file, locks the first, and asserts the second cannot also lock it
/// (`EWOULDBLOCK`). Returns `true` if the fs honors flock; `false` if it does NOT
/// (the caller falls back to `/proc`-only liveness and LOGS the degradation —
/// never silently trusting a no-op lock).
///
/// NOTE (C-3): this box's `state_dir` is on `ext4`/`/dev/vda1` (local disk) ->
/// flock honored; this probe is for portability.
pub fn flock_honored(state_dir: &Path) -> bool {
    let dir = state_dir.join("liveness");
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let probe = dir.join(".flock-honor-probe");
    let fd1 = match open_lock_fd(&probe) {
        Ok(fd) => fd,
        Err(_) => return false,
    };
    // First fd takes the exclusive lock.
    if !matches!(flock_nb_ex(&fd1), FlockResult::Acquired) {
        let _ = std::fs::remove_file(&probe);
        return false;
    }
    // A SECOND, independent fd on the same file must be DENIED (EWOULDBLOCK) if
    // the fs honors cross-fd flock. (Same-process fds still contend under flock.)
    let honored = match open_lock_fd_existing(&probe) {
        Ok(Some(fd2)) => matches!(flock_nb_ex(&fd2), FlockResult::WouldBlock),
        _ => false,
    };
    drop(fd1);
    let _ = std::fs::remove_file(&probe);
    honored
}

// --- internals -------------------------------------------------------------

/// Open (create if absent) the lock file `O_RDWR|O_CREAT, 0600`.
fn open_lock_fd(path: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    Ok(OwnedFd::from(file))
}

/// Open an EXISTING lock file `O_RDWR` (no create). `Ok(None)` on ENOENT.
fn open_lock_fd_existing(path: &Path) -> io::Result<Option<OwnedFd>> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => Ok(Some(OwnedFd::from(file))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Set or clear `FD_CLOEXEC` on an fd (P4 scoped-CLOEXEC).
fn set_cloexec(fd: &OwnedFd, on: bool) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let new = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, new) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

enum FlockResult {
    Acquired,
    WouldBlock,
    Err(io::Error),
}

/// `flock(fd, LOCK_EX|LOCK_NB)` — non-blocking exclusive lock. `EWOULDBLOCK` is
/// mapped to [`FlockResult::WouldBlock`] (a live holder), every other error to
/// [`FlockResult::Err`].
fn flock_nb_ex(fd: &OwnedFd) -> FlockResult {
    let rc = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return FlockResult::Acquired;
    }
    let err = io::Error::last_os_error();
    // EWOULDBLOCK (== EAGAIN on Linux) means a live holder owns the lock.
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        FlockResult::WouldBlock
    } else {
        FlockResult::Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn acquire_then_probe_dead_is_false_while_held() {
        let d = tmp();
        let lock = LivenessLock::acquire(d.path(), "sess-a")
            .expect("acquire io")
            .expect("lock free -> acquired");
        // While the lock is held in-process, a probe must see it LIVE.
        assert!(
            !probe_dead(d.path(), "sess-a"),
            "a held lock must read live (probe_dead=false)"
        );
        drop(lock);
        // After drop the kernel frees the lock -> probe sees it dead.
        assert!(
            probe_dead(d.path(), "sess-a"),
            "after the holder drops, probe_dead must be true (lock free)"
        );
    }

    #[test]
    fn second_acquire_while_held_returns_none() {
        let d = tmp();
        let _held = LivenessLock::acquire(d.path(), "sess-b")
            .expect("acquire io")
            .expect("first acquire");
        // A second acquire of the SAME session must see a live holder -> None.
        let second = LivenessLock::acquire(d.path(), "sess-b").expect("acquire io");
        assert!(
            second.is_none(),
            "a second acquire while held must return None (EWOULDBLOCK)"
        );
    }

    #[test]
    fn probe_dead_on_absent_lock_file_is_false() {
        let d = tmp();
        // No lock file ever created: absence is NOT positive death (fail-closed).
        assert!(
            !probe_dead(d.path(), "never-locked"),
            "an absent lock file must not be read as dead"
        );
    }

    #[test]
    fn lock_path_is_session_keyed() {
        let p = liveness_lock_path(Path::new("/s"), "abc123");
        assert_eq!(p, Path::new("/s/liveness/abc123.lock"));
    }

    #[test]
    fn flock_honored_on_local_tmpfs_or_disk() {
        // The test tempdir is on the box's local fs (tmpfs or ext4); flock is
        // honored there. This is the boot self-test's happy path.
        let d = tmp();
        assert!(
            flock_honored(d.path()),
            "flock must be honored on the local test fs"
        );
    }

    #[test]
    fn cloexec_is_set_by_default_and_clearable() {
        // The held lock fd defaults to FD_CLOEXEC SET (P4); allow_inherit clears it.
        let d = tmp();
        let lock = LivenessLock::acquire(d.path(), "sess-cloexec")
            .expect("acquire io")
            .expect("acquire");
        let raw = lock._fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD must succeed");
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "the lock fd must default to FD_CLOEXEC SET (P4 false-alive fix)"
        );
        // The whitelisted handoff clears it.
        lock.allow_inherit().expect("allow_inherit");
        let flags2 = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(
            flags2 & libc::FD_CLOEXEC == 0,
            "allow_inherit must clear FD_CLOEXEC for the managed-wrapper handoff"
        );
    }
}
