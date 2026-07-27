//! attended/emitter.rs — the authoritative mux-side terminal emitter (RT-R2).
//!
//! After a no-`--wait` sender exits, the resident mux is the SOLE process that can
//! emit a mux-held send's terminal. This emitter is NEW qrmux code that binds the
//! shared `quorum-delivery-events` vocabulary (leaf crate; qrmux already depends on
//! it with DEFAULT features → `preserve_order` ON) and writes ONE terminal per
//! send_id to the AUTHORITATIVE delivery ledger
//! `<state_dir>/sessions/<sessionId>.events.jsonl`, byte-identically to how
//! `dispatch::events` writes it.
//!
//! # It is NOT the advisory stream
//! The qrmux advisory `EventWriter` (`crate::events`, `<socketdir>/events/…`) stays
//! send_id-free and UNTOUCHED. This is a SEPARATE writer to a SEPARATE (the
//! authoritative) ledger, keyed off the pending-delivery handoff that carries the
//! send identity.
//!
//! # Byte-identity (BUILD-DIRECTIVES 2b — the LANDMINE)
//! The wire bytes are produced by `quorum_delivery_events::build_record_line`, which
//! depends on serde_json `preserve_order`. qrmux's dep on the leaf crate uses
//! DEFAULT features (`json-insertion-order`) — NEVER set `default-features = false`,
//! or emission silently sorts keys (non-byte-identical). The mux is a NEW writer:
//! its own `pid`, its own per-file monotonic `seq` from 0, its own `start_ms`;
//! multi-writer is the DESIGNED case (readers merge cross-pid by file order and
//! resolve per send_id). `build_line` is verified byte-identical against the frozen
//! golden (the `quorum-delivery-events/tests/golden_wire.rs` pattern) below.

use quorum_delivery_events::{build_record_line, Envelope, Payload, CHUNK_SHA_CAP};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::Clock;

/// The authoritative mux-side terminal writer for one hosted session's ledger.
pub struct MuxEmitter {
    /// `<state_dir>/sessions/<sessionId>.events.jsonl` (the same authoritative
    /// ledger dispatch's send path writes; readers merge the sessionId + byname
    /// files).
    path: PathBuf,
    /// sessionId (envelope `session`), from the handoff metadata.
    session: Option<String>,
    /// qd session name (envelope `name`), from the handoff metadata.
    name: Option<String>,
    /// Per-(this-writer, file) monotonic seq from 0 (in-memory; a fresh process
    /// restarts at 0 — cross-process disambiguation is by pid+start_ms, exactly as
    /// dispatch's writer). The mux is a distinct writer from qd, so its seq stream
    /// is its own.
    seq: AtomicU64,
    /// This emitting process's OS start-time (epoch ms), stamped on every envelope
    /// (RF-6 dead-writer rule). `None` ⇒ omitted from the wire.
    start_ms: Option<i64>,
}

impl MuxEmitter {
    /// Open an emitter for a session's authoritative ledger. `start_ms` is the
    /// mux's OWN process start (epoch ms) — best-effort; `None` omits the field.
    pub fn new(
        path: impl Into<PathBuf>,
        session: Option<String>,
        name: Option<String>,
        start_ms: Option<i64>,
    ) -> Self {
        Self {
            path: path.into(),
            session,
            name,
            seq: AtomicU64::new(0),
            start_ms,
        }
    }

    /// Emit exactly one record for `payload` to the authoritative ledger, appending
    /// a single byte-exact line (`O_APPEND | O_CREAT`, mode 0600, one `write_all`).
    /// The mux only ever emits TERMINAL kinds, so the line never approaches
    /// `MAX_RECORD_BYTES` (no large `chunk_sha256s` array) and needs no shrink belt.
    pub fn emit(&self, clock: &dyn Clock, payload: &Payload) -> std::io::Result<()> {
        let env = self.envelope(clock.now_ms());
        let line = build_line(&env, payload);
        append_record(&self.path, &line)
    }

    /// Build this writer's envelope for `now_ms` and take the next seq.
    fn envelope(&self, now_ms: i64) -> Envelope {
        Envelope {
            v: 1,
            ts: epoch_ms_to_iso(now_ms),
            pid: std::process::id(),
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            session: self.session.clone(),
            name: self.name.clone(),
            start_ms: self.start_ms,
        }
    }
}

/// Build the byte-exact wire line for a record. The mux emits terminal kinds only,
/// so `CHUNK_SHA_CAP` never binds (no `chunk_sha256s`); this is the plain
/// `build_record_line` path — byte-identical to dispatch's `emit`/`fit_line` for
/// these kinds (proven by the golden test below).
pub fn build_line(env: &Envelope, payload: &Payload) -> String {
    build_record_line(env, payload, CHUNK_SHA_CAP)
}

/// Append one record line to the ledger (`{line}\n`), `O_APPEND | O_CREAT`, mode
/// 0600, one `write_all` — mirrors `dispatch::events::append_record` exactly.
fn append_record(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(format!("{line}\n").as_bytes())
}

// ===========================================================================
// ISO-8601 UTC-ms timestamp — byte-identical port of dispatch's
// render::epoch_ms_to_iso / civil_from_epoch_ms / civil_from_days (std-only, so
// the leaf-crate-free mux emits the SAME `ts` bytes as dispatch).
// ===========================================================================

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (ms precision, always). Byte-identical to
/// `dispatch::render::epoch_ms_to_iso`.
pub fn epoch_ms_to_iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = civil_from_epoch_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

fn civil_from_epoch_ms(ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let total_secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000) as u32;
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let min = ((secs_of_day % 3600) / 60) as u32;
    let sec = (secs_of_day % 60) as u32;
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, hour, min, sec, milli)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quorum_delivery_events::{sha256_hex, Anchor};

    /// The SAME fixed envelope as `quorum-delivery-events/tests/golden_wire.rs`, so
    /// the mux emitter's `build_line` can be checked byte-for-byte against the
    /// frozen golden the vocabulary crate pins.
    fn golden_env() -> Envelope {
        Envelope {
            v: 1,
            ts: "2026-06-06T06:09:00.123Z".to_string(),
            pid: 71234,
            seq: 7,
            session: Some("11111111-2222-3333-4444-555555555555".to_string()),
            name: Some("alpha".to_string()),
            start_ms: Some(1_781_241_500_000),
        }
    }

    fn sha(b: &[u8]) -> String {
        sha256_hex(b)
    }

    /// PRIMARY-SOURCE byte-identity evidence (BUILD-DIRECTIVES 2b): the mux
    /// emitter's `build_line` produces bytes IDENTICAL to the frozen golden for
    /// EVERY terminal kind the mux emits. A break here is an immediate flag.
    #[test]
    fn mux_terminal_bytes_match_frozen_golden() {
        let env = golden_env();
        let cases: Vec<(&str, Payload, &str)> = vec![
            (
                "message-seen",
                Payload::MessageSeen {
                    send_id: "71234-1781241549123-10".to_string(),
                    content_sha256: sha(b"the message"),
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"message-seen","send_id":"71234-1781241549123-10","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825"}"#,
            ),
            (
                "turn-anchored",
                Payload::TurnAnchored {
                    send_id: "71234-1781241549123-3".to_string(),
                    content_sha256: sha(b"the message"),
                    anchor: Anchor {
                        transcript: "/path/to/transcript.jsonl".to_string(),
                        start_offset: 4096,
                        line_index: 42,
                    },
                    recovered: false,
                    attribution: None,
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"turn-anchored","send_id":"71234-1781241549123-3","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","anchor":{"transcript":"/path/to/transcript.jsonl","start_offset":4096,"line_index":42}}"#,
            ),
            (
                "turn-anchored-mismatch",
                Payload::TurnAnchoredMismatch {
                    send_id: "71234-1781241549123-5".to_string(),
                    expected_sha: sha(b"x"),
                    actual_sha: sha(b"y"),
                    expected_len: 100,
                    actual_len: 90,
                    recovered: false,
                    attribution: None,
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"turn-anchored-mismatch","send_id":"71234-1781241549123-5","expected_sha":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881","actual_sha":"a1fce4363854ff888cff4b8e7875d600c2682390412a8cf79b37d0b11148b0fa","expected_len":100,"actual_len":90}"#,
            ),
            (
                "anchor-timeout",
                Payload::AnchorTimeout {
                    send_id: "71234-1781241549123-6".to_string(),
                    waited_ms: 30000,
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"anchor-timeout","send_id":"71234-1781241549123-6","waited_ms":30000}"#,
            ),
            (
                "seen-failed",
                Payload::SeenFailed {
                    send_id: "71234-1781241549123-11".to_string(),
                    reason: "recipient-gone".to_string(),
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"seen-failed","send_id":"71234-1781241549123-11","reason":"recipient-gone"}"#,
            ),
            (
                "send-failed",
                Payload::SendFailed {
                    send_id: Some("71234-1781241549123-12".to_string()),
                    content_sha256: sha(b"the message"),
                    reason: "verify-blocked".to_string(),
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"send-failed","send_id":"71234-1781241549123-12","content_sha256":"c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825","reason":"verify-blocked"}"#,
            ),
            (
                "pending-abandoned",
                Payload::PendingAbandoned {
                    send_id: "71234-1781241549123-7".to_string(),
                    reason: "unknown-inject-outcome".to_string(),
                    recovered: None,
                    attribution: None,
                },
                r#"{"v":1,"ts":"2026-06-06T06:09:00.123Z","pid":71234,"seq":7,"start_ms":1781241500000,"session":"11111111-2222-3333-4444-555555555555","name":"alpha","event":"pending-abandoned","send_id":"71234-1781241549123-7","reason":"unknown-inject-outcome"}"#,
            ),
        ];
        for (label, payload, expected) in cases {
            let got = build_line(&env, &payload);
            assert_eq!(
                got, expected,
                "mux emitter byte-identity mismatch for `{label}`:\n got: {got}\n exp: {expected}"
            );
        }
    }

    #[test]
    fn epoch_ms_to_iso_matches_dispatch() {
        assert_eq!(epoch_ms_to_iso(0), "1970-01-01T00:00:00.000Z");
        // 2026-06-06T06:09:00.123Z → its epoch ms (cross-checks the civil algo).
        // 2026-06-06 is day 20610 from epoch; 06:09:00 = 22140s; .123 = 123ms.
        let ms = (20_610i64 * 86_400 + 22_140) * 1000 + 123;
        assert_eq!(epoch_ms_to_iso(ms), "2026-06-06T06:09:00.123Z");
        // Pre-epoch (negative) stays well-formed via div_euclid/rem_euclid.
        assert_eq!(epoch_ms_to_iso(-1), "1969-12-31T23:59:59.999Z");
    }

    /// End-to-end: the emitter appends exactly one byte-exact line to the ledger
    /// file (O_APPEND), and a second emit appends a second line with the next seq.
    #[test]
    fn emit_appends_one_byte_exact_line_per_record() {
        struct FixedClock(i64);
        impl Clock for FixedClock {
            fn now_ms(&self) -> i64 {
                self.0
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("sid.events.jsonl");
        let em = MuxEmitter::new(
            &path,
            Some("sid".to_string()),
            Some("beta".to_string()),
            Some(1_781_241_500_000),
        );
        let clock = FixedClock(1_781_241_549_123);
        em.emit(
            &clock,
            &Payload::MessageSeen {
                send_id: "s1".to_string(),
                content_sha256: sha(b"m"),
            },
        )
        .unwrap();
        em.emit(
            &clock,
            &Payload::SeenFailed {
                send_id: "s2".to_string(),
                reason: "recipient-gone".to_string(),
            },
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record");
        // Each line is valid JSON and carries the mux's own pid + a monotonic seq
        // from 0.
        assert!(lines[0].contains(r#""seq":0"#), "first seq is 0: {}", lines[0]);
        assert!(lines[1].contains(r#""seq":1"#), "second seq is 1: {}", lines[1]);
        assert!(lines[0].contains(r#""event":"message-seen""#));
        assert!(lines[1].contains(r#""event":"seen-failed""#));
        assert!(lines[0].contains(&format!(r#""pid":{}"#, std::process::id())));
    }
}
