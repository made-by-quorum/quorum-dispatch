//! attended/sweep.rs — the on-boot pending-delivery reconciliation (RT-R1, QS-4).
//!
//! On server start, BEFORE serving, the mux sweeps its session's durable spool and
//! resolves every still-pending send to EXACTLY ONE honest terminal, keyed to the
//! observed PTY-survival fact (the hosted session DIES with the mux, so on a fresh
//! boot after a crash `session_alive == false`). It NEVER re-injects — an unknown
//! inject outcome terminals honestly (`pending-abandoned`), a session-gone draft is
//! retained-and-reported (`seen-failed{recipient-gone}`), a provably-landed send
//! resolves to a late `message-seen`.
//!
//! # First-terminal-wins, under a ledger flock
//! The re-check ("does a terminal for this send_id already exist?") and the emit
//! run UNDER an exclusive advisory `flock` on the ledger file — the same
//! read-check-then-append-under-one-lock idiom as dispatch's `emit_recovery_verdict`
//! (§C2/F2). A late live terminal or a concurrent reconciler cannot produce a second
//! terminal: the second caller blocks, re-reads, observes the first's terminal, and
//! takes the idempotent skip path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quorum_delivery_events::Payload;

use super::emitter::MuxEmitter;
use super::fire::LandingProbe;
use super::spool::{PendingRecord, Spool};
use super::{reconcile, Clock, ReconcileVerdict};

/// Reconcile every spooled pending send for one session's spool to exactly one
/// honest terminal (or a skip / a retained resurface). `ledger_for` resolves a
/// record's authoritative ledger path (QD_HOME-honoring, sessionId-or-byname).
/// Returns the `(send_id, verdict)` for each record (for logging / tests).
pub fn reconcile_spool(
    spool: &Spool,
    clock: &dyn Clock,
    probe: &dyn LandingProbe,
    session_alive: bool,
    ledger_for: &dyn Fn(&PendingRecord) -> PathBuf,
) -> std::io::Result<Vec<(String, ReconcileVerdict)>> {
    let records = spool.load_all()?;
    // One emitter per ledger file so `seq` stays monotonic per writer.
    let mut emitters: HashMap<PathBuf, Arc<MuxEmitter>> = HashMap::new();
    let mut out = Vec::new();

    for rec in records {
        let ledger_path = ledger_for(&rec);
        // Landing is a pure transcript read (never touches the ledger) → outside
        // the lock. Keyed on the durable content sha (reconcile lacks the text).
        let landing = probe
            .scan_sha(
                rec.transcript.as_deref(),
                rec.transcript_offset,
                &rec.content_sha256,
                rec.content_len,
            )
            .to_result();

        // The re-check → emit critical section, serialized across processes.
        let verdict = {
            let _lock = LedgerLock::acquire(&ledger_path)?;
            let already = ledger_has_terminal(&ledger_path, &rec.send_id);
            let verdict = reconcile(rec.phase, landing, session_alive, already);
            if let Some(payload) = verdict_payload(&verdict, &rec) {
                let emitter = emitters.entry(ledger_path.clone()).or_insert_with(|| {
                    let start_ms =
                        crate::procid::pid_start_ms(std::process::id()).map(|v| v as i64);
                    Arc::new(MuxEmitter::new(
                        ledger_path.clone(),
                        rec.session.clone(),
                        rec.name.clone(),
                        start_ms,
                    ))
                });
                if let Err(e) = emitter.emit(clock, &payload) {
                    tracing::warn!(send_id = %rec.send_id, error = %e,
                        "reconcile terminal emit failed");
                }
            }
            verdict
        };

        // Clear the spool for every RESOLVED send. A ResurfaceDraft (live session —
        // never on this box, where the session dies with the mux) is KEPT for the
        // M2 banner recovery path.
        match verdict {
            ReconcileVerdict::ResurfaceDraft => {}
            _ => {
                let _ = spool.remove(&rec.send_id);
            }
        }
        out.push((rec.send_id, verdict));
    }
    Ok(out)
}

/// Map a reconcile verdict to the terminal payload to emit (or `None` — skip /
/// resurface). Terminal kinds come from the shared vocabulary (never minted).
/// `pub(crate)` so the wired byte-identity gate can prove the reconcile path's
/// payloads serialize golden-identically.
pub(crate) fn verdict_payload(verdict: &ReconcileVerdict, rec: &PendingRecord) -> Option<Payload> {
    match verdict {
        // A terminal already exists (first-terminal-wins) — emit nothing.
        ReconcileVerdict::AlreadyTerminal => None,
        // Live session (not this box) — the draft re-surfaces via the M2 banner;
        // no terminal yet.
        ReconcileVerdict::ResurfaceDraft => None,
        ReconcileVerdict::LateMessageSeen => Some(Payload::MessageSeen {
            send_id: rec.send_id.clone(),
            content_sha256: rec.content_sha256.clone(),
        }),
        ReconcileVerdict::SeenFailedRecipientGone => Some(Payload::SeenFailed {
            send_id: rec.send_id.clone(),
            reason: "recipient-gone".to_string(),
        }),
        ReconcileVerdict::PendingAbandonedUnknown => Some(Payload::PendingAbandoned {
            send_id: rec.send_id.clone(),
            reason: "unknown-inject-outcome".to_string(),
            recovered: None,
            attribution: None,
        }),
        // M1's sha-only reconcile never yields a truncation Mismatch (it lacks the
        // message bytes), so LateMismatch is unreachable here; handle defensively as
        // an honest abandoned rather than fabricating anchor detail. (M4/M5 refine
        // reconcile-time truncation detection.)
        ReconcileVerdict::LateMismatch => Some(Payload::PendingAbandoned {
            send_id: rec.send_id.clone(),
            reason: "unknown-inject-outcome".to_string(),
            recovered: None,
            attribution: None,
        }),
    }
}

/// Does the ledger already carry a TERMINAL record for `send_id`? A best-effort
/// scan (a torn/unreadable line is skipped). Absent/unreadable ledger ⇒ false.
fn ledger_has_terminal(ledger_path: &Path, send_id: &str) -> bool {
    let body = match std::fs::read_to_string(ledger_path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let matches_id = v.get("send_id").and_then(|s| s.as_str()) == Some(send_id);
        // Consume the leaf vocab's canonical terminal set — never a locally-minted
        // copy (F3 / consume-don't-fork). A new 8th terminal kind is recognized here
        // for free, keeping first-terminal-wins honest (QS-4/RT-R1).
        let is_terminal = v
            .get("event")
            .and_then(|e| e.as_str())
            .map(quorum_delivery_events::is_terminal)
            .unwrap_or(false);
        if matches_id && is_terminal {
            return true;
        }
    }
    false
}

/// Exclusive advisory `flock(LOCK_EX)` on the ledger file, held across the
/// re-check→emit and released on drop. Mirrors dispatch's `RecoveryEmitLock`
/// (§C2/F2). Advisory: only contends with OTHER flock holders (the normal
/// `O_APPEND` emission sites are unaffected).
struct LedgerLock {
    _flock: nix::fcntl::Flock<std::fs::File>,
}

impl LedgerLock {
    fn acquire(ledger_path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = ledger_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(ledger_path)?;
        // Blocking exclusive lock — a second reconciler waits here, then its
        // re-check observes the first's terminal.
        let flock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| std::io::Error::from(e))?;
        Ok(Self { _flock: flock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attended::fire::LandingScan;
    use crate::attended::{FirePhase, SystemClock};

    struct FixedShaProbe(LandingScan);
    impl LandingProbe for FixedShaProbe {
        fn scan(&self, _t: Option<&str>, _o: Option<u64>, _m: &str) -> LandingScan {
            LandingScan::Unconfirmed
        }
        fn scan_sha(
            &self,
            _t: Option<&str>,
            _o: Option<u64>,
            _s: &str,
            _l: u64,
        ) -> LandingScan {
            self.0.clone()
        }
    }

    fn spool_with(records: &[PendingRecord]) -> (tempfile::TempDir, Spool) {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path().join("pending")).unwrap();
        for r in records {
            spool.write(r).unwrap();
        }
        (dir, spool)
    }

    fn rec(send_id: &str, phase: FirePhase) -> PendingRecord {
        let mut r = PendingRecord::accepted(
            send_id,
            quorum_delivery_events::sha256_hex(b"hi"),
            2,
            Some("sid".into()),
            Some("alpha".into()),
            "send:pty",
            false,
            0,
        );
        r.phase = phase;
        r.fire_started = matches!(phase, FirePhase::FireStarted | FirePhase::FireCompleted);
        r.fire_completed = matches!(phase, FirePhase::FireCompleted);
        r
    }

    fn ledger_for<'a>(dir: &'a Path) -> impl Fn(&PendingRecord) -> PathBuf + 'a {
        move |r: &PendingRecord| {
            crate::attended::driver::ledger_path(dir, r.session.as_deref(), "alpha")
        }
    }

    // ---- session-gone (this box): inject-not-run ⇒ seen-failed, never re-inject

    #[test]
    fn accepted_send_session_gone_reconciles_to_seen_failed_and_clears_spool() {
        let (_d, spool) = spool_with(&[rec("s1", FirePhase::Accepted)]);
        let ledger_dir = tempfile::tempdir().unwrap();
        let out = reconcile_spool(
            &spool,
            &SystemClock,
            &FixedShaProbe(LandingScan::Unconfirmed),
            false, // session dies with the mux
            &ledger_for(ledger_dir.path()),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, ReconcileVerdict::SeenFailedRecipientGone);
        // Spool cleared; the ledger carries exactly one seen-failed terminal.
        assert!(spool.load("s1").unwrap().is_none());
        let led = crate::attended::driver::ledger_path(ledger_dir.path(), Some("sid"), "alpha");
        let body = std::fs::read_to_string(&led).unwrap();
        assert_eq!(body.lines().filter(|l| l.contains("\"event\":\"seen-failed\"")).count(), 1);
    }

    // ---- unknown inject (crash mid-fire) + unconfirmed ⇒ pending-abandoned, no reinject

    #[test]
    fn fire_started_unconfirmed_reconciles_to_pending_abandoned() {
        let (_d, spool) = spool_with(&[rec("s2", FirePhase::FireStarted)]);
        let ledger_dir = tempfile::tempdir().unwrap();
        let out = reconcile_spool(
            &spool,
            &SystemClock,
            &FixedShaProbe(LandingScan::Unconfirmed),
            false,
            &ledger_for(ledger_dir.path()),
        )
        .unwrap();
        assert_eq!(out[0].1, ReconcileVerdict::PendingAbandonedUnknown);
    }

    // ---- fire completed + sha proves landed ⇒ late message-seen

    #[test]
    fn fire_completed_landed_reconciles_to_late_message_seen() {
        let (_d, spool) = spool_with(&[rec("s3", FirePhase::FireCompleted)]);
        let ledger_dir = tempfile::tempdir().unwrap();
        let out = reconcile_spool(
            &spool,
            &SystemClock,
            &FixedShaProbe(LandingScan::Landed),
            false,
            &ledger_for(ledger_dir.path()),
        )
        .unwrap();
        assert_eq!(out[0].1, ReconcileVerdict::LateMessageSeen);
        let led = crate::attended::driver::ledger_path(ledger_dir.path(), Some("sid"), "alpha");
        assert!(std::fs::read_to_string(&led)
            .unwrap()
            .contains("\"event\":\"message-seen\""));
    }

    // ---- idempotence: a pre-existing terminal ⇒ AlreadyTerminal, no second write

    #[test]
    fn existing_terminal_is_idempotent_no_second_emit() {
        let (_d, spool) = spool_with(&[rec("s4", FirePhase::Accepted)]);
        let ledger_dir = tempfile::tempdir().unwrap();
        let led = crate::attended::driver::ledger_path(ledger_dir.path(), Some("sid"), "alpha");
        std::fs::create_dir_all(led.parent().unwrap()).unwrap();
        // A terminal for s4 already exists in the ledger.
        std::fs::write(
            &led,
            "{\"v\":1,\"event\":\"seen-failed\",\"send_id\":\"s4\",\"reason\":\"recipient-gone\"}\n",
        )
        .unwrap();
        let out = reconcile_spool(
            &spool,
            &SystemClock,
            &FixedShaProbe(LandingScan::Unconfirmed),
            false,
            &ledger_for(ledger_dir.path()),
        )
        .unwrap();
        assert_eq!(out[0].1, ReconcileVerdict::AlreadyTerminal);
        // Still exactly ONE terminal line (no duplicate).
        let body = std::fs::read_to_string(&led).unwrap();
        assert_eq!(body.lines().filter(|l| l.contains("\"send_id\":\"s4\"")).count(), 1);
        // Spool cleared (resolved).
        assert!(spool.load("s4").unwrap().is_none());
    }

    // ---- live session (other box): inject-not-run + alive ⇒ resurface, KEEP spooled

    #[test]
    fn accepted_send_live_session_resurfaces_and_keeps_spool() {
        let (_d, spool) = spool_with(&[rec("s5", FirePhase::Countdown)]);
        let ledger_dir = tempfile::tempdir().unwrap();
        let out = reconcile_spool(
            &spool,
            &SystemClock,
            &FixedShaProbe(LandingScan::Unconfirmed),
            true, // hypothetical live session
            &ledger_for(ledger_dir.path()),
        )
        .unwrap();
        assert_eq!(out[0].1, ReconcileVerdict::ResurfaceDraft);
        // Draft retained for the banner recovery path — NOT cleared, NO terminal.
        assert!(spool.load("s5").unwrap().is_some());
    }
}
