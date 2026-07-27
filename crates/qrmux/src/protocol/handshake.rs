//! Wire-format version handshake.
//!
//! WHY THIS EXISTS (war story — do not delete): protocol-version skew is a named
//! failure class and one of the reasons this rewrite exists (see `exec/b2-spec.md`
//! deliverable #3: "protocol carries a version byte from day 1"). A client and
//! daemon built from different commits can disagree about the bincode layout of
//! `ClientMsg`/`ServerMsg`; without a version gate the daemon would misparse
//! frames into the wrong variants — silent corruption, not a clean error.
//!
//! DESIGN: a fixed 5-byte preamble (4 magic bytes + 1 version byte) sent by the
//! client immediately after connect, BEFORE any length-prefixed frames. The
//! preamble's shape is FROZEN FOREVER — it must never grow, shrink, or move —
//! precisely so that any future version can always read any past version's
//! preamble. Everything after the preamble is version-gated.
//!
//! REFUSAL CONTRACT: on mismatch the server replies with a framed
//! `ServerMsg::Error` (variant index 4 — frozen, see PROTOCOL.md "frozen
//! surface") and closes. Clean refusal, never a hang, never a silent drop.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Current protocol version. Per the v3 AMENDED versioning rule (PROTOCOL.md
/// §3.3): bump only on BREAKING changes (mutating an existing variant/field
/// layout, the frame format, or Hello semantics). Additive evolution =
/// append a variant/field + a new capability string, NO bump.
///
/// **v2 (C1 D1):** added `ClientMsg::GetHistory`, `ClientMsg::CreateDetached`,
/// and the additive `SessionInfo::created` field — all layout changes, hence
/// the bump from 1.
///
/// **v3 (WS-C):** added the `Hello` capability-exchange frame (appended
/// `ClientMsg::Hello` / `ServerMsg::Hello`) and made the Hello-first handshake
/// normative on every connection — a Hello-semantics change, hence the bump.
/// The 5-byte preamble shape and `ServerMsg::Error`'s variant index 4 are
/// FROZEN and unchanged. See PROTOCOL.md "Versioning rule".
///
/// **v4 (P4DB drive-burn):** removed `ClientMsg::LaunchHeadless` (the one-off
/// `claude -p` stream-json drive verb). It sat in the MIDDLE of `ClientMsg`
/// (immediately before `SubscribeRepublish`), so removing it SHIFTS
/// `SubscribeRepublish`'s positional bincode index (12 → 11) — a layout-mutating
/// BREAKING change, hence the bump from 3. The 5-byte preamble and
/// `ServerMsg::Error` variant index 4 stay FROZEN. With the bump, a pre-burn (v3)
/// and post-burn (v4) peer REFUSE CLEANLY at the version gate (→ the surviving
/// caller's documented fallback, e.g. `qd wait`'s disk-poll / the per-session
/// "stale qrmux daemon — restart THAT session" surfacing) instead of SILENTLY
/// misframing `SubscribeRepublish` as the removed verb.
///
/// **v5 (attended-UX M1):** APPENDED the polite-delivery surface at the tail of
/// both message enums — `ClientMsg::PendingDelivery`/`DeliverNow` and
/// `ServerMsg::DeliveryQueued`/`DeliveryOutcome`. Appending preserves every
/// existing positional bincode index (including the FROZEN `ServerMsg::Error`
/// index 4), so this is layout-additive rather than layout-mutating; the bump to
/// 5 still ensures a skewed peer refuses cleanly at the version gate rather than
/// misframing a v5-only frame. The 5-byte preamble stays FROZEN.
pub const PROTOCOL_VERSION: u8 = 5;

/// Additive capability for cell-exact, logical-line-framed history transport.
/// Peers that do not advertise it continue to use `ServerMsg::History`.
pub const HISTORY_LOGICAL_V1_CAP: &str = "history-logical-v1";

/// Additive completion contract for logical-history responses that span more
/// than one protocol frame. Peers advertising this capability accumulate
/// non-empty `HistoryLogical` frames until an empty `HistoryLogical` frame,
/// which is the explicit completion marker.
pub const HISTORY_LOGICAL_STREAM_V1_CAP: &str = "history-logical-stream-v1";

/// Additive attach-handshake contract: after `Connected`, the client samples
/// its terminal again and sends `ConfirmSize` before the server snapshots and
/// emits logical history plus the initial full repaint.
pub const INITIAL_SIZE_CONFIRM_V1_CAP: &str = "initial-size-confirm-v1";

/// Magic bytes identifying an qrmux client connection. Frozen forever.
pub const PREAMBLE_MAGIC: [u8; 4] = *b"QRMX";

/// Total preamble length on the wire: magic + version byte.
pub const PREAMBLE_LEN: usize = 5;

/// Result of reading a client preamble on the server side.
#[derive(Debug, PartialEq)]
pub enum PreambleCheck {
    /// Magic + version both match: proceed with framed protocol.
    Ok,
    /// Peer closed before sending a full preamble (e.g. liveness probes that
    /// connect-and-drop). Not an error; close quietly.
    Eof,
    /// First 4 bytes were not the qrmux magic — not an qrmux client.
    BadMagic([u8; 4]),
    /// Magic matched but version differs from ours.
    VersionMismatch { client: u8 },
}

/// Write the client-side preamble. Must be the first bytes on the wire.
pub async fn write_preamble<W: AsyncWriteExt + Unpin>(w: &mut W) -> std::io::Result<()> {
    let mut buf = [0u8; PREAMBLE_LEN];
    buf[..4].copy_from_slice(&PREAMBLE_MAGIC);
    buf[4] = PROTOCOL_VERSION;
    w.write_all(&buf).await
}

/// Read and validate the client preamble (server side). Reads exactly
/// [`PREAMBLE_LEN`] bytes; maps clean EOF to [`PreambleCheck::Eof`] so probe
/// connections don't log as errors.
pub async fn read_preamble<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<PreambleCheck> {
    let mut buf = [0u8; PREAMBLE_LEN];
    let mut filled = 0;
    while filled < PREAMBLE_LEN {
        let n = r.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Ok(PreambleCheck::Eof);
        }
        filled += n;
    }
    if buf[..4] != PREAMBLE_MAGIC {
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&buf[..4]);
        return Ok(PreambleCheck::BadMagic(magic));
    }
    if buf[4] != PROTOCOL_VERSION {
        return Ok(PreambleCheck::VersionMismatch { client: buf[4] });
    }
    Ok(PreambleCheck::Ok)
}

/// Framed-error string the server returns when the FIRST frame after the
/// preamble is not [`crate::protocol::ClientMsg::Hello`] (PROTOCOL.md §3.2
/// step 2). Exact-equality contract — tests assert on the whole string.
pub const ERR_EXPECTED_HELLO: &str = "protocol error: expected Hello as first frame";

/// Maximum number of capability strings in a Hello frame (defensive bound on a
/// pre-auth frame; PROTOCOL.md §3.2).
pub const HELLO_MAX_CAPS: usize = 64;
/// Maximum byte length of a single capability string.
pub const HELLO_MAX_CAP_LEN: usize = 64;

/// Validate the `caps` list of a `ClientMsg::Hello` against the §3.2 defensive
/// bounds: ≤[`HELLO_MAX_CAPS`] entries, each ≤[`HELLO_MAX_CAP_LEN`] bytes and
/// matching the kebab-case charset `[a-z0-9-]+` (non-empty). On violation
/// returns the exact framed-error string the server must send before closing.
pub fn validate_hello_caps(caps: &[String]) -> Result<(), String> {
    if caps.len() > HELLO_MAX_CAPS {
        return Err(format!(
            "protocol error: Hello caps count {} exceeds max {}",
            caps.len(),
            HELLO_MAX_CAPS
        ));
    }
    for cap in caps {
        if cap.len() > HELLO_MAX_CAP_LEN {
            return Err(format!(
                "protocol error: Hello cap {} bytes exceeds max {}",
                cap.len(),
                HELLO_MAX_CAP_LEN
            ));
        }
        if cap.is_empty()
            || !cap
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!(
                "protocol error: Hello cap '{}' not kebab-case [a-z0-9-]+",
                cap
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn preamble_round_trip_ok() {
        let (mut w, mut r) = tokio::io::duplex(64);
        write_preamble(&mut w).await.unwrap();
        assert_eq!(read_preamble(&mut r).await.unwrap(), PreambleCheck::Ok);
    }

    #[tokio::test]
    async fn preamble_eof_on_immediate_close() {
        let (w, mut r) = tokio::io::duplex(64);
        drop(w);
        assert_eq!(read_preamble(&mut r).await.unwrap(), PreambleCheck::Eof);
    }

    #[tokio::test]
    async fn preamble_eof_on_partial_write() {
        let (mut w, mut r) = tokio::io::duplex(64);
        w.write_all(b"QR").await.unwrap();
        drop(w);
        assert_eq!(read_preamble(&mut r).await.unwrap(), PreambleCheck::Eof);
    }

    #[tokio::test]
    async fn preamble_bad_magic() {
        let (mut w, mut r) = tokio::io::duplex(64);
        w.write_all(b"NOPE\x01").await.unwrap();
        match read_preamble(&mut r).await.unwrap() {
            PreambleCheck::BadMagic(m) => assert_eq!(&m, b"NOPE"),
            other => panic!("expected BadMagic, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn preamble_version_mismatch() {
        let (mut w, mut r) = tokio::io::duplex(64);
        let mut buf = [0u8; PREAMBLE_LEN];
        buf[..4].copy_from_slice(&PREAMBLE_MAGIC);
        buf[4] = PROTOCOL_VERSION + 1; // future client
        w.write_all(&buf).await.unwrap();
        match read_preamble(&mut r).await.unwrap() {
            PreambleCheck::VersionMismatch { client } => {
                assert_eq!(client, PROTOCOL_VERSION + 1)
            }
            other => panic!("expected VersionMismatch, got {:?}", other),
        }
    }

    /// v5 negotiation arm (attended-UX M1 bump; updated from the prior v4 pin):
    /// this server is now v5 (the polite-delivery surface appended), so a stale
    /// v4 client's preamble must be reported as a mismatch carrying the v4 client
    /// byte — the deploy skew window the spec calls out. The adapter surfaces
    /// this PER-SESSION as "stale qrmux daemon for session '<name>'; kill or
    /// restart THAT session".
    #[tokio::test]
    async fn preamble_v4_client_refused_by_v5_server() {
        assert_eq!(
            PROTOCOL_VERSION, 5,
            "this test pins the v5 bump; update it on the next bump"
        );
        let (mut w, mut r) = tokio::io::duplex(64);
        let mut buf = [0u8; PREAMBLE_LEN];
        buf[..4].copy_from_slice(&PREAMBLE_MAGIC);
        buf[4] = 4; // a stale v4 (pre-attended) client/daemon
        w.write_all(&buf).await.unwrap();
        match read_preamble(&mut r).await.unwrap() {
            PreambleCheck::VersionMismatch { client } => assert_eq!(client, 4),
            other => panic!("expected VersionMismatch, got {:?}", other),
        }
    }

    /// v5 negotiation arm: a current (v5) client preamble is accepted by a v5
    /// server — the positive twin of the skew refusal above.
    #[tokio::test]
    async fn preamble_v5_client_accepted_by_v5_server() {
        let (mut w, mut r) = tokio::io::duplex(64);
        write_preamble(&mut w).await.unwrap();
        assert_eq!(read_preamble(&mut r).await.unwrap(), PreambleCheck::Ok);
    }

    #[test]
    fn validate_hello_caps_empty_ok() {
        assert!(validate_hello_caps(&[]).is_ok());
    }

    #[test]
    fn validate_hello_caps_valid_kebab_ok() {
        let caps = vec![
            "foo".to_string(),
            "foo-bar".to_string(),
            "v3-thing-2".to_string(),
        ];
        assert!(validate_hello_caps(&caps).is_ok());
    }

    #[test]
    fn validate_hello_caps_too_many() {
        let caps: Vec<String> = (0..HELLO_MAX_CAPS + 1).map(|_| "x".to_string()).collect();
        assert!(validate_hello_caps(&caps).is_err());
    }

    #[test]
    fn validate_hello_caps_too_long() {
        let caps = vec!["a".repeat(HELLO_MAX_CAP_LEN + 1)];
        assert!(validate_hello_caps(&caps).is_err());
    }

    #[test]
    fn validate_hello_caps_bad_charset() {
        // uppercase, underscore, and empty are all rejected.
        assert!(validate_hello_caps(&["Foo".to_string()]).is_err());
        assert!(validate_hello_caps(&["foo_bar".to_string()]).is_err());
        assert!(validate_hello_caps(&["".to_string()]).is_err());
    }
}
