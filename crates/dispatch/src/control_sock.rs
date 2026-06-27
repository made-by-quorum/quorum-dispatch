//! Per-session `AF_UNIX` `SOCK_DGRAM` control socket (R3c-Step-1, R1 §5).
//!
//! A best-effort, never-blocking wake/control channel keyed on `session_id`. The
//! servicer (the per-session qrmux daemon) BINDS the receiving end adjacent to its
//! `<name>.sock` listener (`qrmux/src/server/mod.rs`, the confirmed design-(A) seam)
//! and services it in its existing `tokio::select!`; this module is the SENDER side
//! plus the shared path + wire definitions.
//!
//! ## Why session_id, not pid (R1 §5 inv 4)
//! The path keys on `session_id` — STABLE across an incarnation bump (a Rung-4
//! respawn mints a fresh pid + incarnation but keeps the session_id), so a sender
//! computes the same control path before and after a respawn without re-resolving
//! the registry row.
//!
//! ## Wire format (≤64B fixed-size datagrams)
//! A single opcode byte. The qrmux servicer decodes the SAME opcodes from a mirror
//! const block (`server/mod.rs` `ctrl_op`): there is NO shared crate to host the
//! type because `dispatch` depends on `qrmux`, not the reverse — so the opcode
//! values are duplicated by necessity and a comment on each side points at the
//! other. Keep [`OP_WAKE_INBOX`]..[`OP_GRACEFUL_STOP`] in lockstep with that mirror.
//!
//! ## Never-blocks (R1 §5 inv 2, OQ#2)
//! [`send_control`] uses an unbound, non-blocking datagram socket: a full receive
//! buffer yields `WouldBlock`, which we treat as delivered-best-effort (the datagram
//! is dropped; the inbox turn-completion drain and the ladder's Rung-2 re-send are
//! the reliability backstops). ONLY a genuinely absent servicer (`ENOENT` — no bound
//! socket file) or a dead one (`ECONNREFUSED`) surfaces as an `Err`, so the enqueue
//! hook can fall back to the direct PTY inject (the load-bearing-path proof).

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// The control-socket path for `session_id`: `<state_dir>/control/<session_id>.sock`.
///
/// Keyed on `session_id` (not pid) so it is stable across an incarnation bump
/// (R1 §5 inv 4). The servicer binds this; senders `sendto` it. The `control/`
/// subdir is created lazily by the binder (the daemon), not here.
pub fn control_sock_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join("control").join(format!("{session_id}.sock"))
}

/// Fixed-size control messages, serialized as a single opcode byte (≤64B datagram).
///
/// `WakeInbox` is the R3c-Step-1 payload (new inbox mail → nudge the agent);
/// `Ping`/`Checkpoint`/`GracefulStop` are the recovery-ladder + lifecycle opcodes
/// (Rung 2 sends `WakeInbox` + `Checkpoint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMsg {
    /// New inbox mail was enqueued — drain/notice it (R3c-Step-1).
    WakeInbox,
    /// Liveness probe (the servicer may answer by advancing progress).
    Ping,
    /// Request a best-effort checkpoint write (recovery Rung 2 / pre-respawn).
    Checkpoint,
    /// Request an orderly turn-boundary stop (lifecycle).
    GracefulStop,
}

// --- wire opcodes (1 byte) -------------------------------------------------
// MIRRORED in `qrmux/src/server/mod.rs` (`ctrl_op` const block). Change BOTH
// together — there is no shared crate (dispatch depends on qrmux, not vice versa).
/// Opcode for [`ControlMsg::WakeInbox`].
pub const OP_WAKE_INBOX: u8 = 1;
/// Opcode for [`ControlMsg::Ping`].
pub const OP_PING: u8 = 2;
/// Opcode for [`ControlMsg::Checkpoint`].
pub const OP_CHECKPOINT: u8 = 3;
/// Opcode for [`ControlMsg::GracefulStop`].
pub const OP_GRACEFUL_STOP: u8 = 4;

impl ControlMsg {
    /// The 1-byte wire opcode.
    pub fn opcode(self) -> u8 {
        match self {
            ControlMsg::WakeInbox => OP_WAKE_INBOX,
            ControlMsg::Ping => OP_PING,
            ControlMsg::Checkpoint => OP_CHECKPOINT,
            ControlMsg::GracefulStop => OP_GRACEFUL_STOP,
        }
    }

    /// The fixed-size (1-byte) datagram payload.
    pub fn to_bytes(self) -> [u8; 1] {
        [self.opcode()]
    }

    /// Decode an opcode byte. `None` for an unknown opcode (the servicer ignores
    /// unknown control datagrams rather than crashing — forward-compat).
    pub fn from_opcode(b: u8) -> Option<ControlMsg> {
        match b {
            OP_WAKE_INBOX => Some(ControlMsg::WakeInbox),
            OP_PING => Some(ControlMsg::Ping),
            OP_CHECKPOINT => Some(ControlMsg::Checkpoint),
            OP_GRACEFUL_STOP => Some(ControlMsg::GracefulStop),
            _ => None,
        }
    }
}

/// Best-effort, NEVER-blocking send of one control datagram to `path` (R1 §5 inv 2).
///
/// Returns `Ok(())` when the datagram was handed to the kernel OR dropped because
/// the receiver's buffer was momentarily full (`WouldBlock` — best-effort, no
/// retry, OQ#2: the inbox drain + ladder Rung-2 re-send are the backstops). Returns
/// `Err` ONLY when there is no live servicer — `NotFound` (`ENOENT`, no bound
/// socket) or `ConnectionRefused` (`ECONNREFUSED`, servicer dead) — so the enqueue
/// hook can fall back to the direct PTY inject and LOG (proving the socket path is
/// load-bearing, R3c-1 negative control).
pub fn send_control(path: &Path, msg: ControlMsg) -> io::Result<()> {
    let sock = UnixDatagram::unbound()?;
    // Non-blocking: a full receive buffer must NEVER block the sender (R1 §5 inv 2).
    sock.set_nonblocking(true)?;
    match sock.send_to(&msg.to_bytes(), path) {
        Ok(_) => Ok(()),
        // Buffer momentarily full → drop best-effort (sender never blocks). The
        // turn-completion inbox drain + the ladder's Rung-2 re-send cover the drop.
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
        // ENOENT / ECONNREFUSED (and any other genuine failure) surface so the
        // caller falls back to PTY-inject.
        Err(e) => Err(e),
    }
}

/// The action a wake-on-enqueue took, for logging + the load-bearing-path proof
/// (R3c-Step-1 negative control, §3 R3c-1 row).
///
/// `ControlSocket` is the happy path: the always-serviced control fd carried the
/// wake. `PtyFallback` means there was no live servicer (`ENOENT`/`ECONNREFUSED`) so
/// the caller must fall back to a direct PTY inject (and LOG) — the existence of
/// this branch, and the caller acting on it, is exactly what makes the socket path
/// load-bearing: revert the enqueue hook (or kill the ctrl reader) and the wake
/// degrades here instead of riding the always-serviced fd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// The `WakeInbox` datagram was delivered on the daemon-serviced control fd.
    ControlSocket,
    /// No live servicer — the caller must PTY-inject + log. Carries the reason.
    PtyFallback { reason: String },
}

/// Send a `WakeInbox` to the session's control socket and decide the fallback.
///
/// NEVER blocks (best-effort datagram). Returns [`WakeOutcome::ControlSocket`] when
/// the datagram was handed off (or best-effort dropped on a momentarily-full
/// buffer), or [`WakeOutcome::PtyFallback`] when there is no live servicer
/// (`ENOENT`/`ECONNREFUSED`) — the caller then PTY-injects + logs. This is the
/// enqueue hook's decision core; the production caller (the relay server, after
/// persisting the inbox file) acts on the outcome.
pub fn wake_inbox(ctrl_path: &Path) -> WakeOutcome {
    match send_control(ctrl_path, ControlMsg::WakeInbox) {
        Ok(()) => WakeOutcome::ControlSocket,
        Err(e) => WakeOutcome::PtyFallback {
            reason: format!("{e} ({:?})", e.kind()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn path_is_keyed_on_session_id_under_control_subdir() {
        let p = control_sock_path(Path::new("/s/state"), "abc-123");
        assert_eq!(p, Path::new("/s/state/control/abc-123.sock"));
        // Stable across incarnation: the same session_id yields the same path
        // regardless of pid/incarnation (R1 §5 inv 4).
        let p2 = control_sock_path(Path::new("/s/state"), "abc-123");
        assert_eq!(p, p2);
    }

    #[test]
    fn opcode_round_trips_for_every_variant() {
        for msg in [
            ControlMsg::WakeInbox,
            ControlMsg::Ping,
            ControlMsg::Checkpoint,
            ControlMsg::GracefulStop,
        ] {
            let b = msg.to_bytes();
            assert_eq!(b.len(), 1, "fixed-size 1-byte datagram");
            assert_eq!(ControlMsg::from_opcode(b[0]), Some(msg));
        }
    }

    #[test]
    fn unknown_opcode_decodes_to_none() {
        assert_eq!(ControlMsg::from_opcode(0), None);
        assert_eq!(ControlMsg::from_opcode(200), None);
    }

    #[test]
    fn send_control_delivers_opcode_to_a_bound_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rx.sock");
        let rx = UnixDatagram::bind(&path).unwrap();
        rx.set_nonblocking(true).unwrap();

        send_control(&path, ControlMsg::WakeInbox).expect("send to bound receiver");

        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).expect("receive datagram");
        assert_eq!(n, 1);
        assert_eq!(ControlMsg::from_opcode(buf[0]), Some(ControlMsg::WakeInbox));
    }

    #[test]
    fn wake_inbox_rides_the_control_socket_when_a_servicer_is_bound() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("control")).unwrap();
        let ctrl = control_sock_path(dir.path(), "sess-1");
        let rx = UnixDatagram::bind(&ctrl).unwrap();
        rx.set_nonblocking(true).unwrap();

        // A bound servicer → the wake rides the always-serviced fd.
        assert_eq!(wake_inbox(&ctrl), WakeOutcome::ControlSocket);
        // And it really delivered the WakeInbox opcode (non-vacuous).
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).expect("servicer received the wake datagram");
        assert_eq!(ControlMsg::from_opcode(buf[0]), Some(ControlMsg::WakeInbox));
        assert_eq!(n, 1);
    }

    #[test]
    fn wake_inbox_falls_back_when_no_servicer_is_bound() {
        // No socket file at all (servicer never bound / daemon dead + unlinked) →
        // ENOENT → the load-bearing fallback decision (the §3 R3c-1 negative
        // control: revert the hook / kill the reader → wake degrades to here).
        let dir = tempfile::tempdir().unwrap();
        let ctrl = control_sock_path(dir.path(), "sess-absent");
        match wake_inbox(&ctrl) {
            WakeOutcome::PtyFallback { reason } => {
                assert!(
                    reason.contains("NotFound"),
                    "fallback reason should name the ENOENT cause, got: {reason}"
                );
            }
            other => panic!("expected PtyFallback when no servicer is bound, got {other:?}"),
        }
    }

    #[test]
    fn send_control_to_absent_socket_is_enoent_for_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control").join("no-such.sock");
        let err = send_control(&path, ControlMsg::WakeInbox)
            .expect_err("absent servicer must surface an error so the hook falls back");
        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "ENOENT (no bound socket) must surface, not be swallowed"
        );
    }
}
