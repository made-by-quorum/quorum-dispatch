//! PTY-write fault-injection layer (ACK-1, ack1-spec §4.1) — TEST SEAM.
//!
//! Sits between the SendInput handler and the raw PTY write, BELOW the event
//! emitter: the emitter reports the result the daemon OBSERVED, which is
//! exactly the deception injection 3 (silent swallow) needs.
//!
//! ALWAYS COMPILED, env-armed at daemon start, loudly logged when armed.
//! Rationale (orc-approved, checkpoint ruling item 1): the tested binary stays
//! the shipped binary — no gates-green-on-a-build-variant gap; a fault
//! injector can only make things RED (it cannot launder a failure green, the
//! ADD-12(4) class); org precedent = the env-armed RETACH_B1_BREAK breaker.
//! Arming requires explicit env on the daemon process; unset = a no-op
//! identity layer (the R-NEG negative control).
//!
//! Env surface (read once at daemon start):
//! - `QRMUX_FAULT_PTY_WRITE` = `error` | `swallow`
//! - `QRMUX_FAULT_ERRNO`     = int (default 5 = EIO), for `error` mode
//! - `QRMUX_FAULT_DROP_FRAMES` = `send-input` (park-open frame drop)
//! - `QRMUX_FAULT_SESSION`   = exact session-name filter (default: all)
//! - `QRMUX_FAULT_MATCH_SHA256` = 64-hex content filter (default: all)
//!
//! Filters AND together. A dropped frame must look like SILENCE (the engine's
//! T_write timeout is the intended consumer signature, rev C §4 injection 1):
//! the handler PARKS the connection open instead of replying or closing.

use std::sync::Arc;

/// Write-path fault mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WriteFault {
    /// The PTY write returns Err(errno) WITHOUT writing (injection 2).
    Error,
    /// The write is skipped but reported Ok — the emitter records
    /// `pty-bytes-written` and the client gets `InputSent`, yet NO bytes
    /// reach the PTY (injection 3).
    Swallow,
}

/// Parsed fault configuration. `Default` (all `None`/off) = identity.
#[derive(Debug, Default)]
pub struct FaultLayer {
    write_mode: Option<WriteFault>,
    errno: i32,
    drop_send_input_frames: bool,
    session_filter: Option<String>,
    sha_filter: Option<String>,
}

impl FaultLayer {
    /// Parse the env surface once. Logs the LOUD arming warn when any mode is
    /// set (rider R-a: the warn is an executable gate assertion — present in
    /// captured daemon stderr when armed, absent when not).
    pub fn from_env() -> Arc<Self> {
        let write_mode = match std::env::var("QRMUX_FAULT_PTY_WRITE").ok().as_deref() {
            Some("error") => Some(WriteFault::Error),
            Some("swallow") => Some(WriteFault::Swallow),
            Some(other) => {
                tracing::warn!(value = %other, "QRMUX_FAULT_PTY_WRITE unrecognized — fault layer NOT armed");
                None
            }
            None => None,
        };
        let errno = std::env::var("QRMUX_FAULT_ERRNO")
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(5); // EIO
        let drop_send_input_frames =
            std::env::var("QRMUX_FAULT_DROP_FRAMES").ok().as_deref() == Some("send-input");
        let session_filter = std::env::var("QRMUX_FAULT_SESSION").ok();
        let sha_filter = std::env::var("QRMUX_FAULT_MATCH_SHA256").ok();

        let layer = Self {
            write_mode,
            errno,
            drop_send_input_frames,
            session_filter,
            sha_filter,
        };
        if layer.armed() {
            tracing::warn!(
                write_mode = ?layer.write_mode,
                errno = layer.errno,
                drop_frames = layer.drop_send_input_frames,
                session_filter = ?layer.session_filter,
                sha_filter = ?layer.sha_filter,
                "FAULT INJECTION ARMED — this daemon is a TEST instance; PTY writes will be faulted"
            );
        }
        Arc::new(layer)
    }

    /// Any fault mode active?
    pub fn armed(&self) -> bool {
        self.write_mode.is_some() || self.drop_send_input_frames
    }

    /// Filters (session AND sha) match this frame?
    fn matches(&self, session: &str, content_sha256: &str) -> bool {
        if let Some(s) = &self.session_filter {
            if s != session {
                return false;
            }
        }
        if let Some(h) = &self.sha_filter {
            if h != content_sha256 {
                return false;
            }
        }
        true
    }

    /// Injection 1: should this SendInput frame be dropped at handler entry
    /// (no validate, no write, no event, no reply — connection parked open)?
    pub fn should_drop_frame(&self, session: &str, content_sha256: &str) -> bool {
        self.drop_send_input_frames && self.matches(session, content_sha256)
    }

    /// Injections 2/3: intercept the PTY write. `None` = pass through to the
    /// real write; `Some(result)` = the faulted outcome (the caller emits
    /// events from it exactly as from a real result).
    pub fn intercept_write(
        &self,
        session: &str,
        content_sha256: &str,
    ) -> Option<std::io::Result<()>> {
        let mode = self.write_mode?;
        if !self.matches(session, content_sha256) {
            return None;
        }
        Some(match mode {
            WriteFault::Error => Err(std::io::Error::from_raw_os_error(self.errno)),
            WriteFault::Swallow => Ok(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(
        write_mode: Option<WriteFault>,
        drop: bool,
        session: Option<&str>,
        sha: Option<&str>,
    ) -> FaultLayer {
        FaultLayer {
            write_mode,
            errno: 5,
            drop_send_input_frames: drop,
            session_filter: session.map(String::from),
            sha_filter: sha.map(String::from),
        }
    }

    /// Identity when unarmed (the R-NEG control's unit form).
    #[test]
    fn unarmed_is_identity() {
        let f = FaultLayer::default();
        assert!(!f.armed());
        assert!(!f.should_drop_frame("s", "x"));
        assert!(f.intercept_write("s", "x").is_none());
    }

    /// error mode returns the configured errno; swallow returns Ok.
    #[test]
    fn write_modes() {
        let f = layer(Some(WriteFault::Error), false, None, None);
        let err = f.intercept_write("s", "x").unwrap().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(5));
        let f = layer(Some(WriteFault::Swallow), false, None, None);
        assert!(f.intercept_write("s", "x").unwrap().is_ok());
    }

    /// Filters AND together; non-matching frames pass through untouched.
    #[test]
    fn filters_and_together() {
        let f = layer(Some(WriteFault::Swallow), true, Some("s1"), Some("abc"));
        assert!(f.should_drop_frame("s1", "abc"));
        assert!(!f.should_drop_frame("s1", "zzz"));
        assert!(!f.should_drop_frame("s2", "abc"));
        assert!(f.intercept_write("s1", "abc").is_some());
        assert!(f.intercept_write("s2", "abc").is_none());
    }
}
