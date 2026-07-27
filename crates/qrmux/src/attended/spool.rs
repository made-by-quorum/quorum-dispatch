//! attended/spool.rs — the durable pending-delivery store (RT-R1).
//!
//! Per-session durable store of in-flight ("pending") sends the mux is holding or
//! firing, so a mux crash never loses the human's words and every spooled send
//! reconciles on restart to EXACTLY ONE honest terminal (QS-4). Lives under the
//! per-session runtime dir (`<socket_dir>/pending/<session>/<send_id>.json`).
//!
//! # Write discipline (BUILD-DIRECTIVES 2c)
//! **Per-EVENT writes, NEVER per-keystroke fsync.** The three machinery-owned write
//! points are: (1) acceptance (write-ahead — the send is durable before the sender
//! gets its queued receipt); (2) countdown-start draft snapshot, refreshed at
//! fire-start BEFORE the clear-chord; (3) terminal-or-reconcile on restart. Each
//! write is atomic (tmp + fsync + rename) so a crash mid-write never yields a torn
//! record.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

use super::FirePhase;

/// One spooled pending send. Carries everything a restart reconciliation needs to
/// resolve it to a terminal WITHOUT the original sender (which may have exited):
/// the send identity, the content hash/len, the transcript window keys, and the
/// preserved human **draft** (P4 — retained for recovery/report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingRecord {
    /// The qd-minted send_id (`"{pid}-{epoch_ms}-{n}"`) carried on the handoff.
    pub send_id: String,
    /// sha256 of the canonical sent text (for landing correlation).
    pub content_sha256: String,
    /// UTF-8 byte length of the sent text.
    pub content_len: u64,
    /// sessionId (ledger key) + qd name, from the handoff.
    pub session: Option<String>,
    pub name: Option<String>,
    /// The initiating verb ("send:pty" | "new-p").
    pub verb: String,
    /// Resolved transcript path + pre-fire offset — the landing/recovery window.
    pub transcript: Option<String>,
    pub transcript_offset: Option<u64>,
    /// Priority flag (shortens the countdown ceiling).
    pub priority: bool,
    /// Send-acceptance instant (epoch ms) — the countdown ceiling anchor.
    pub accepted_at_ms: i64,
    /// Durable fire progress.
    pub phase: FirePhase,
    /// The preserved human draft snapshot (P4). Empty until countdown-start.
    #[serde(with = "b64")]
    pub draft: Vec<u8>,
    /// `fire_started` durable BEFORE the clear-chord (inject MAY have run).
    pub fire_started: bool,
    /// `fire_completed` durable after a confirmed successful inject.
    pub fire_completed: bool,
}

impl PendingRecord {
    /// A fresh write-ahead record at acceptance (write point 1).
    pub fn accepted(
        send_id: impl Into<String>,
        content_sha256: impl Into<String>,
        content_len: u64,
        session: Option<String>,
        name: Option<String>,
        verb: impl Into<String>,
        priority: bool,
        accepted_at_ms: i64,
    ) -> Self {
        Self {
            send_id: send_id.into(),
            content_sha256: content_sha256.into(),
            content_len,
            session,
            name,
            verb: verb.into(),
            transcript: None,
            transcript_offset: None,
            priority,
            accepted_at_ms,
            phase: FirePhase::Accepted,
            draft: Vec::new(),
            fire_started: false,
            fire_completed: false,
        }
    }
}

/// The per-session durable pending-delivery store.
pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    /// Open (creating) the store dir `<socket_dir>/pending/<session>/`.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn record_path(&self, send_id: &str) -> PathBuf {
        // send_id is a mux/qd-minted token ("{pid}-{ms}-{n}") — filename-safe.
        self.dir.join(format!("{send_id}.json"))
    }

    /// Durably write (create or overwrite) a pending record — atomic tmp + fsync +
    /// rename (per-event; a crash mid-write leaves the prior record intact, never a
    /// torn one). Used at all three write points.
    pub fn write(&self, rec: &PendingRecord) -> std::io::Result<()> {
        let body = serde_json::to_vec(rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.dir.join(format!(".{}.json.tmp", rec.send_id));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?; // per-EVENT fsync (never per-keystroke)
        }
        std::fs::rename(&tmp, self.record_path(&rec.send_id))
    }

    /// Load a single record by send_id, if present.
    pub fn load(&self, send_id: &str) -> std::io::Result<Option<PendingRecord>> {
        match std::fs::read(self.record_path(send_id)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e)
            })?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Load every spooled record (for on-restart reconciliation). Skips the `.tmp`
    /// scratch files. An unreadable or undeserializable record at an authoritative
    /// path is skipped AND `warn!`-logged here (adv-r1 O1: the prior "logged by the
    /// caller" claim was false — the reconcile never saw the silent drop). Such a
    /// record is OUTSIDE the atomic-write durability model (tmp+fsync+rename precludes
    /// a torn AUTHORITATIVE record; only external corruption reaches it), so it is a
    /// LOW observability residual, not an in-model failure. It is deliberately NOT
    /// quarantined to a terminal here: that would make this store a terminal WRITER
    /// (violating the mux's single-writer-per-send_id invariant), and there is no
    /// existing leaf kind for "corrupt spool record" to reuse without minting one
    /// (anti-fork). The record is left in place; the `warn!` re-surfaces it on every
    /// boot so it can never dangle SILENTLY.
    pub fn load_all(&self) -> std::io::Result<Vec<PendingRecord>> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for entry in rd {
            let entry = entry?;
            let path = entry.path();
            let is_json = path.extension().map(|e| e == "json").unwrap_or(false);
            let is_tmp = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false);
            if !is_json || is_tmp {
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<PendingRecord>(&bytes) {
                    Ok(rec) => out.push(rec),
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "attended spool: skipping an undeserializable/corrupt pending record \
                         on load_all — it reconciles to ZERO terminals (external corruption, \
                         outside the atomic-write model); left in place, re-warned each boot"
                    ),
                },
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "attended spool: skipping an unreadable pending record on load_all"
                ),
            }
        }
        Ok(out)
    }

    /// Remove a record once it has resolved to a terminal (idempotent).
    pub fn remove(&self, send_id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.record_path(send_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Compact base64-ish encoding for the draft bytes in the record JSON (avoids a
/// bulky number-array and keeps arbitrary bytes round-trippable). std-only.
mod b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map_err(serde::de::Error::custom)
    }

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        fn val(c: u8) -> Result<u32, String> {
            match c {
                b'A'..=b'Z' => Ok((c - b'A') as u32),
                b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
                b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err("bad base64 char".to_string()),
            }
        }
        let s = s.as_bytes();
        if s.len() % 4 != 0 {
            return Err("bad base64 length".to_string());
        }
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        for chunk in s.chunks(4) {
            let pad = chunk.iter().filter(|&&c| c == b'=').count();
            let n = (val(chunk[0])? << 18)
                | (val(chunk[1])? << 12)
                | (if chunk[2] == b'=' { 0 } else { val(chunk[2])? } << 6)
                | (if chunk[3] == b'=' { 0 } else { val(chunk[3])? });
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> PendingRecord {
        PendingRecord::accepted(
            "71234-1781241549123-0",
            "c9946a075fd077dde6476a4669e543ca6bcd59064ccc1173477f7b4c9d005825",
            11,
            Some("sid-1".to_string()),
            Some("alpha".to_string()),
            "send:pty",
            false,
            1_781_241_549_123,
        )
    }

    #[test]
    fn write_ahead_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path().join("pending").join("sid-1")).unwrap();
        let r = rec();
        spool.write(&r).unwrap();
        let got = spool.load(&r.send_id).unwrap().unwrap();
        assert_eq!(got, r);
        assert_eq!(got.phase, FirePhase::Accepted);
    }

    #[test]
    fn draft_snapshot_round_trips_byte_exact_incl_arbitrary_bytes() {
        // P4/QS-2: the preserved draft survives a durable round-trip byte-for-byte,
        // including non-UTF8 / control bytes (base64 field).
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path()).unwrap();
        let mut r = rec();
        r.phase = FirePhase::Countdown;
        r.draft = (0u16..=255).map(|b| b as u8).collect(); // every byte value
        r.draft.extend_from_slice("héllo\nworld".as_bytes());
        spool.write(&r).unwrap();
        let got = spool.load(&r.send_id).unwrap().unwrap();
        assert_eq!(got.draft, r.draft, "draft must round-trip byte-exact");
    }

    #[test]
    fn phase_transitions_persist_via_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path()).unwrap();
        let mut r = rec();
        spool.write(&r).unwrap();
        // countdown-start: snapshot the draft.
        r.phase = FirePhase::Countdown;
        r.draft = b"in-progress".to_vec();
        spool.write(&r).unwrap();
        // fire-start BEFORE clear-chord.
        r.phase = FirePhase::FireStarted;
        r.fire_started = true;
        spool.write(&r).unwrap();
        let got = spool.load(&r.send_id).unwrap().unwrap();
        assert_eq!(got.phase, FirePhase::FireStarted);
        assert!(got.fire_started);
        assert_eq!(got.draft, b"in-progress");
    }

    #[test]
    fn load_all_scans_records_and_ignores_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path()).unwrap();
        for i in 0..3 {
            let mut r = rec();
            r.send_id = format!("s-{i}");
            spool.write(&r).unwrap();
        }
        // A stray tmp scratch file must be ignored by load_all.
        std::fs::write(dir.path().join(".s-99.json.tmp"), b"partial").unwrap();
        let all = spool.load_all().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn load_all_skips_a_corrupt_authoritative_record_and_keeps_the_valid_sibling() {
        // adv-r1 O1: a corrupt/truncated record at the AUTHORITATIVE path (not a .tmp
        // scratch) is SKIPPED — it reconciles to zero terminals — and no longer
        // silently: `load_all` `warn!`s it (see the fn doc). A valid sibling still
        // loads (the sweep is not aborted by the corrupt one). The corrupt file is
        // left in place (never quarantined to a terminal — that would need a second
        // writer / a minted kind).
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path()).unwrap();
        let ok = rec();
        spool.write(&ok).unwrap();
        // A corrupt/truncated record at the authoritative path.
        std::fs::write(
            dir.path().join("s-corrupt.json"),
            br#"{"send_id":"s-corrupt","content_sha256":"deadbeef"#,
        )
        .unwrap();
        let all = spool.load_all().unwrap();
        assert_eq!(all.len(), 1, "only the valid sibling loads; the corrupt one is skipped");
        assert_eq!(all[0].send_id, ok.send_id);
        assert!(
            dir.path().join("s-corrupt.json").exists(),
            "the corrupt record is left in place (not quarantined to a terminal)"
        );
    }

    #[test]
    fn remove_on_terminal_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path()).unwrap();
        let r = rec();
        spool.write(&r).unwrap();
        spool.remove(&r.send_id).unwrap();
        assert!(spool.load(&r.send_id).unwrap().is_none());
        spool.remove(&r.send_id).unwrap(); // idempotent
    }

    #[test]
    fn base64_round_trips_all_byte_values() {
        for len in 0..20 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(b64::decode(&b64::encode(&bytes)).unwrap(), bytes);
        }
    }
}
