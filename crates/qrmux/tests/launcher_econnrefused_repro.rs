//! Repro PIN for the ECONNREFUSED-under-load launcher flake (eng-harden lane,
//! off `71e89cf`). Root cause (prior round, eng-hardening-backlog.md §"persistent
//! flake"): `server_launcher::ensure_session_server_running` did a SINGLE-SHOT
//! `probe_liveness`; under CPU saturation a transient `ECONNREFUSED` against a
//! full accept backlog of a LIVE per-session daemon was misclassified
//! `Liveness::Crashed`, the launcher UNLINKED the live socket under the flock and
//! spawned a capacity-1 replacement that could not bind → the 5s budget exhausted
//! → `bail!("failed to start daemon")` (`c1_gate.rs:356`, roaming victim row).
//!
//! Deterministic simulation (no CPU saturation needed — the exact mechanism, the
//! discovery-lane `scan_transient_refusal_survives_and_returns_row` trick): an
//! AF_UNIX socket BOUND but not yet LISTENING refuses connect with `ECONNREFUSED`
//! identically to a full backlog; calling `listen()` on the SAME fd flips it to
//! accepting with no unlink/rebind window. A helper thread flips at +150ms — INSIDE
//! the launcher's death-confirmation budget (probes at ~0 / ~100 / ~350ms) — then
//! speaks the launcher probe protocol (ServerHello{session} + an empty History so
//! `GetHistory` reads `Liveness::Up`).
//!
//! PIN: post-fix `ensure_session_server_running` reads the daemon `Up` on a
//! confirmation retry, returns `Ok(())`, and the live socket SURVIVES (never
//! unlinked, no replacement spawned). Pre-fix (single-shot) it unlinks the live
//! socket and bails — RED on BOTH the `Ok` and the socket-survives assertions.

use std::error::Error;
use std::time::Duration;

use qrmux::client::server_launcher::{ensure_session_server_running, ServerLaunchSpec};

/// Flip-to-listening delay: inside the death-confirmation window (probes land at
/// ~0 / ~100 / ~350ms), 50ms past the second probe and 200ms before the third —
/// the same margin the discovery-lane pin uses.
const FLIP_AFTER_MS: u64 = 150;

#[tokio::test]
async fn launcher_transient_refusal_survives_no_wrong_victim_unlink() -> Result<(), Box<dyn Error>>
{
    use nix::libc;
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream as StdUnixStream;

    let name = "alpha";
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join(format!("{name}.sock"));

    // Bind WITHOUT listen: connects fail ECONNREFUSED (the full-backlog shape).
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(
            fd >= 0,
            "socket() failed: {}",
            std::io::Error::last_os_error()
        );
        // Bound accept() so the helper never blocks forever if the launcher (a
        // regressed single-shot launcher) unlinks the socket and never connects.
        let tv = libc::timeval {
            tv_sec: 3,
            tv_usec: 0,
        };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path_bytes = socket.as_os_str().as_encoded_bytes();
        assert!(path_bytes.len() < std::mem::size_of_val(&addr.sun_path));
        for (i, b) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let rc = libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        assert_eq!(rc, 0, "bind failed: {}", std::io::Error::last_os_error());
        fd
    };
    assert!(socket.exists(), "bound socket file must exist");

    // Fake LIVE daemon: refuse (bound, not listening) until +150ms, then flip to
    // LISTENING on the same fd and answer the launcher's session-addressed probe
    // (ServerHello{session:alpha} + an empty History → Up).
    let daemon = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(FLIP_AFTER_MS));
        unsafe {
            assert_eq!(
                libc::listen(fd, 16),
                0,
                "listen failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let cfd = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if cfd >= 0 {
            let mut s = unsafe { StdUnixStream::from_raw_fd(cfd) };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 1024];
            // Drain the request bytes (preamble + Hello) without assuming they
            // arrive in any particular read-boundary alignment.
            let _ = s.read(&mut buf);
            let hello = qrmux::protocol::encode(&qrmux::protocol::ServerMsg::Hello {
                caps: vec![],
                session: name.into(),
            })
            .unwrap();
            let _ = s.write_all(&hello).and_then(|_| s.flush());
            // The launcher's History loop reads-THEN-decodes, so History must land
            // as a FRESH read AFTER it has consumed the ServerHello and sent its
            // GetHistory. Send it as a separate, briefly-delayed frame (decoupled
            // from any read/write lockstep, which a split preamble+Hello read would
            // desync): the delay is orders of magnitude above the launcher's
            // microsecond ServerHello→GetHistory turnaround.
            std::thread::sleep(Duration::from_millis(40));
            let history =
                qrmux::protocol::encode(&qrmux::protocol::ServerMsg::History(vec![])).unwrap();
            let _ = s.write_all(&history).and_then(|_| s.flush());
            // Best-effort drain of the launcher's GetHistory so close() sends a
            // clean FIN, not an RST on unread bytes.
            let _ = s.read(&mut buf);
        }
        unsafe { libc::close(fd) };
    });

    // A harmless launch program: if a REGRESSED launcher reaches the spawn path
    // (it unlinked the socket first), this exits immediately and never binds, so
    // the readiness poll exhausts and the call bails — the RED outcome. The fixed
    // launcher never reaches spawn (the fast-path confirmed probe returns Up).
    let launch = ServerLaunchSpec {
        program: std::path::PathBuf::from("/bin/true"),
        args_prefix: vec![],
    };

    let result = ensure_session_server_running(Some(dir.path()), name, Some(&launch)).await;

    // Surface a helper-thread panic (listen/accept errno) before the assertions.
    daemon.join().expect("daemon thread panicked");

    result.expect("launcher must read the transiently-refusing daemon Up, not bail");
    assert!(
        socket.exists(),
        "WRONG VICTIM: the transiently-refusing LIVE daemon's socket was unlinked"
    );

    Ok(())
}

/// NEGATIVE CONTROL (red-team: the fix must NOT mask a real death). A socket that
/// is bound-but-never-listening refuses EVERY probe — the signature of a
/// genuinely dead daemon that left a stale socket. Death-confirmation must still
/// reach `Liveness::Crashed` (consistent refusal across all three probes), so the
/// launcher UNLINKS the stale socket under the flock and attempts a relaunch. The
/// guard downgrades a SINGLE refusal, never a consistent one — honest failure is
/// delayed ~350ms, never turned into a false-alive.
///
/// Proof: the call BAILS (the `/bin/true` "replacement" never binds) AND the
/// stale socket is GONE — positive evidence the launcher confirmed death and
/// reaped it, rather than reading the dead socket Up. (Runs the full ~5s launch
/// budget by design — the post-spawn readiness poll exhausts when no real daemon
/// comes up.)
#[tokio::test]
async fn launcher_consistent_refusal_is_still_death_not_masked() -> Result<(), Box<dyn Error>> {
    use nix::libc;

    let name = "beta";
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join(format!("{name}.sock"));

    // Bind WITHOUT listen and NEVER flip: every probe refuses (a truly dead
    // daemon's stale socket). Hold the fd for the whole test so the path keeps
    // refusing (ECONNREFUSED) rather than vanishing (ENOENT) until the launcher
    // itself unlinks it.
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(
            fd >= 0,
            "socket() failed: {}",
            std::io::Error::last_os_error()
        );
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path_bytes = socket.as_os_str().as_encoded_bytes();
        assert!(path_bytes.len() < std::mem::size_of_val(&addr.sun_path));
        for (i, b) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let rc = libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        assert_eq!(rc, 0, "bind failed: {}", std::io::Error::last_os_error());
        fd
    };
    assert!(socket.exists(), "bound socket file must exist");

    let launch = ServerLaunchSpec {
        program: std::path::PathBuf::from("/bin/true"),
        args_prefix: vec![],
    };
    let result = ensure_session_server_running(Some(dir.path()), name, Some(&launch)).await;
    unsafe { libc::close(fd) };

    assert!(
        result.is_err(),
        "a consistently-refusing (dead) socket must NOT be read alive — death masked"
    );
    assert!(
        !socket.exists(),
        "death CONFIRMED: the stale socket must be unlinked + relaunch attempted, \
         not left untouched"
    );

    Ok(())
}
