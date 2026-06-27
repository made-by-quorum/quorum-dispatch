//! `RelayPresence` — the bounded stable-read interceptor (WP-B, bug #2).
//!
//! ## The bug
//! The warranty belt (`lifecycle.rs:586`) reads `~/.claude.json` ONCE and nags
//! when the user-scope `relay` MCP key is absent. But the writer set is **claude
//! processes only** (`qd` never writes that file — it mutates only via
//! `claude mcp add/remove`): concurrent claude sessions share the user-global
//! file and atomically temp+fsync+rename it ~10×/turn with **no cross-process
//! lock**. A lost-update — a session writing back a config snapshot it read
//! *before* the relay was registered — transiently clobbers the relay key. A
//! single read that lands in that window sees `Some(false)` and emits a **false
//! nag**. An advisory lock is decorative (claude won't take it); the only lever
//! is **read-side robustness**.
//!
//! ## The §6.1 contract (memo #2)
//! Make `user_relay_registered` consumption a **bounded stable-read** behind ONE
//! `RelayPresence` entry-point (§6.4): re-read until the parsed relay-key value
//! is **stable across a bounded window (k attempts / T ms)**. Then:
//! - a lone **transient absent ⇒ inconclusive** (one clobber can't survive a
//!   unanimous re-read) — **say nothing**;
//! - **persistent absent ⇒ warn** (`Absent`);
//! - **instability ⇒ say nothing** (no false nag);
//! - **parse-fail / unreadable ⇒ None ⇒ no-nag** (the pre-existing contract,
//!   preserved: a malformed or missing config never nags).
//!
//! ## §6.0 posture
//! This hardens the **existing** `~/.claude.json` read (config-PRESENCE, the
//! registration intent — not a real-time coordination signal); it introduces no
//! NEW off-label disk-as-status read. The guarantee is **PROBABILISTIC** (the
//! bounded re-read shrinks the lost-update false-absence; it cannot fully defeat
//! a non-cooperating clobberer — memo #2.3). Torn/truncated reads are already
//! closed by claude's atomic rename (the file is always valid JSON).
//!
//! (B) does not touch this: relay registration is a config concern, not a
//! turn/liveness signal.

use crate::relay_server::register::user_relay_registered;

/// The stable-read verdict the consumer keys on. Only `Absent` warns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayVerdict {
    /// Stable `Some(true)` across the window — the relay is registered.
    Registered,
    /// Stable `Some(false)` across the window — persistently absent. The ONLY
    /// verdict that nags (the false-negative the belt must still catch).
    Absent,
    /// The window did not stabilise on a value: a transient absent, a flap, or a
    /// run of unreadable/unparseable reads. **Say nothing** (no false nag).
    Inconclusive,
}

impl RelayVerdict {
    /// The consumer's decision: warn iff persistently `Absent`.
    pub fn should_warn(self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// The ONE §6.4 interceptor. (B) leaves it untouched; tests inject a scripted
/// reader/sleeper.
pub trait RelayPresence {
    fn check(&self) -> RelayVerdict;
}

/// k — attempts in the bounded window (≥2 so a lone transient absent cannot
/// stand alone). The first read plus the re-reads.
pub const STABLE_READ_ATTEMPTS: usize = 3;
/// T — spacing between attempts (ms). The window is `(k-1)·T` ≈ 50ms — bounded,
/// well under the ~10-writes/turn churn so a present steady state reads stable.
pub const STABLE_READ_SPACING_MS: u64 = 25;

/// Reads the raw config bytes. `None` = unreadable (ENOENT / I/O error) — folded
/// identically to unparseable (no-nag). Seam so tests drive a scripted/churning
/// source without touching the real home (L9a).
pub trait ConfigReader {
    fn read(&self) -> Option<String>;
}

/// Sleep seam (tests pass a no-op so the bounded window costs nothing).
pub trait Sleep {
    fn sleep_ms(&self, ms: u64);
}

/// Real `~/.claude.json` reader, pinned to an injected path (never the real
/// `homedir()` — L9a; the bin layer passes `home.join(".claude.json")`).
pub struct ClaudeJsonReader {
    path: std::path::PathBuf,
}

impl ClaudeJsonReader {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl ConfigReader for ClaudeJsonReader {
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }
}

/// Real sleep.
pub struct RealSleep;

impl Sleep for RealSleep {
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// The bounded stable-read `RelayPresence` impl.
pub struct StableRelayPresence<R: ConfigReader, S: Sleep> {
    reader: R,
    sleeper: S,
    attempts: usize,
    spacing_ms: u64,
}

impl StableRelayPresence<ClaudeJsonReader, RealSleep> {
    /// Production constructor: the real `~/.claude.json` at `path`, real sleep,
    /// default k/T.
    pub fn for_claude_json(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            reader: ClaudeJsonReader::new(path),
            sleeper: RealSleep,
            attempts: STABLE_READ_ATTEMPTS,
            spacing_ms: STABLE_READ_SPACING_MS,
        }
    }
}

impl<R: ConfigReader, S: Sleep> StableRelayPresence<R, S> {
    /// Construct over injected seams + an explicit window (tests).
    pub fn with(reader: R, sleeper: S, attempts: usize, spacing_ms: u64) -> Self {
        Self {
            reader,
            sleeper,
            attempts: attempts.max(1),
            spacing_ms,
        }
    }

    /// One read → the parsed relay-key value. `None` for unreadable OR
    /// unparseable (both no-nag); `Some(true/false)` for present/absent.
    fn read_once(&self) -> Option<bool> {
        self.reader
            .read()
            .as_deref()
            .and_then(user_relay_registered)
    }
}

impl<R: ConfigReader, S: Sleep> RelayPresence for StableRelayPresence<R, S> {
    fn check(&self) -> RelayVerdict {
        let mut reads = Vec::with_capacity(self.attempts);
        for i in 0..self.attempts {
            if i > 0 {
                self.sleeper.sleep_ms(self.spacing_ms);
            }
            reads.push(self.read_once());
        }
        fold(&reads)
    }
}

/// Fold a bounded window of parsed reads into a verdict. **Absent (warn) ONLY
/// on unanimous `Some(false)`** — a persistent, stable absence; ANY disagreement
/// (a transient absent amid present reads, a flap, or a `None`) suppresses the
/// nag. Unanimous `Some(true)` ⇒ Registered. Everything else ⇒ Inconclusive.
/// An empty window is Inconclusive (defensive; `attempts` is clamped to ≥1).
pub fn fold(reads: &[Option<bool>]) -> RelayVerdict {
    if reads.is_empty() {
        return RelayVerdict::Inconclusive;
    }
    if reads.iter().all(|r| *r == Some(false)) {
        RelayVerdict::Absent
    } else if reads.iter().all(|r| *r == Some(true)) {
        RelayVerdict::Registered
    } else {
        RelayVerdict::Inconclusive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A scripted reader: returns queued contents in order, last sticks. `None`
    /// entries model an unreadable read.
    struct ScriptReader {
        seq: Vec<Option<String>>,
        idx: Cell<usize>,
    }
    impl ScriptReader {
        fn new(seq: Vec<Option<String>>) -> Self {
            Self {
                seq,
                idx: Cell::new(0),
            }
        }
    }
    impl ConfigReader for ScriptReader {
        fn read(&self) -> Option<String> {
            let i = self.idx.get();
            let v = self.seq.get(i).or_else(|| self.seq.last()).cloned();
            self.idx.set(i + 1);
            v.flatten()
        }
    }

    struct NoSleep;
    impl Sleep for NoSleep {
        fn sleep_ms(&self, _ms: u64) {}
    }

    const PRESENT: &str = r#"{"mcpServers":{"relay":{"command":"qd","args":["relay:serve"]}}}"#;
    const ABSENT: &str = r#"{"mcpServers":{"playwright":{}}}"#;

    fn presence(seq: Vec<Option<&str>>, attempts: usize) -> RelayVerdict {
        let reader = ScriptReader::new(seq.into_iter().map(|o| o.map(String::from)).collect());
        StableRelayPresence::with(reader, NoSleep, attempts, 0).check()
    }

    // ---- fold logic (the load-bearing semantics) ----

    #[test]
    fn fold_partitions_the_window() {
        use RelayVerdict::*;
        assert_eq!(fold(&[Some(false), Some(false), Some(false)]), Absent);
        assert_eq!(fold(&[Some(true), Some(true), Some(true)]), Registered);
        // A single transient absent amid present reads ⇒ Inconclusive (no nag).
        assert_eq!(fold(&[Some(true), Some(false), Some(true)]), Inconclusive);
        // Flap / instability ⇒ Inconclusive.
        assert_eq!(fold(&[Some(false), Some(true), Some(false)]), Inconclusive);
        // A None (unreadable/unparseable) anywhere ⇒ no unanimous-false ⇒ no nag.
        assert_eq!(fold(&[None, Some(false), Some(false)]), Inconclusive);
        assert_eq!(fold(&[None, None, None]), Inconclusive);
        assert_eq!(fold(&[]), Inconclusive);
    }

    /// CONSUMER-EXERCISING RED-without / GREEN-with (the lost-update false nag).
    /// The window is [absent (the transient clobber), present, present] — a
    /// genuine lost-update blip. With the bounded stable-read (k=3) the consumer
    /// reads PAST the blip ⇒ Inconclusive ⇒ NO warn (GREEN). The fix-shaped
    /// mutation "drop the re-read" (k=1) catches the blip ⇒ Absent ⇒ false nag
    /// (RED). Same input; the re-read is the load-bearing difference.
    #[test]
    fn transient_absent_not_nagged_but_single_read_would() {
        let window = vec![Some(ABSENT), Some(PRESENT), Some(PRESENT)];
        // GREEN — the fix (k=3 stable-read): the transient blip is outvoted.
        assert_eq!(presence(window.clone(), 3), RelayVerdict::Inconclusive);
        assert!(!presence(window.clone(), 3).should_warn(), "no false nag");
        // RED — the mutation (k=1, drop the re-read): the single read IS the blip.
        assert_eq!(presence(window.clone(), 1), RelayVerdict::Absent);
        assert!(
            presence(window, 1).should_warn(),
            "a single read catches the lost-update window ⇒ false nag (the bug)"
        );
    }

    /// FALSE-NEGATIVE guard: a genuinely-absent relay (stable across the whole
    /// window — no writer ever adds the key) STILL warns. The belt must not go
    /// silent on the real P0-cutover failure it exists to catch.
    #[test]
    fn persistent_absent_still_warns() {
        let v = presence(vec![Some(ABSENT), Some(ABSENT), Some(ABSENT)], 3);
        assert_eq!(v, RelayVerdict::Absent);
        assert!(v.should_warn());
    }

    /// FALSE-POSITIVE guard (registered): a stably-present relay never nags.
    #[test]
    fn persistent_present_never_warns() {
        let v = presence(vec![Some(PRESENT), Some(PRESENT), Some(PRESENT)], 3);
        assert_eq!(v, RelayVerdict::Registered);
        assert!(!v.should_warn());
    }

    /// Contract preserved: parse-fail / unreadable ⇒ None ⇒ no-nag, even when
    /// persistent.
    #[test]
    fn unreadable_or_unparseable_never_warns() {
        assert!(!presence(vec![None, None, None], 3).should_warn());
        assert!(!presence(
            vec![Some("{not json"), Some("{not json"), Some("{not json")],
            3
        )
        .should_warn());
    }

    // ---- real-file concurrent atomic-replace (k≥2 writers) + latency ----

    /// Atomically replace `path` with `body` (claude's temp+rename shape) so a
    /// concurrent reader only ever sees a complete, valid JSON (old or new). The
    /// temp name is UNIQUE per call (the writers race on the SAME target, never
    /// on a shared temp).
    fn atomic_write(path: &std::path::Path, body: &str) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp{}.{uniq}", std::process::id()));
        std::fs::write(&tmp, body).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    /// CONCURRENCY (DoD #5, k≥2): two writer threads hammer the user-global file
    /// with atomic renames — BOTH steady-state versions keep the relay present
    /// (the realistic churn: every committed snapshot has the relay). The
    /// stable-read must survive the concurrent replacement with NO torn read, NO
    /// panic, and NEVER a spurious `Absent` (no false nag under load).
    #[test]
    fn concurrent_atomic_replace_never_false_nags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        atomic_write(&path, PRESENT);
        let v1 = r#"{"mcpServers":{"relay":{"command":"qd"},"a":{}}}"#;
        let v2 = r#"{"mcpServers":{"b":{},"relay":{"command":"qd"}}}"#;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut writers = Vec::new();
        for body in [v1, v2] {
            let p = path.clone();
            let s = stop.clone();
            let b = body.to_string();
            writers.push(std::thread::spawn(move || {
                while !s.load(std::sync::atomic::Ordering::Relaxed) {
                    atomic_write(&p, &b);
                }
            }));
        }
        let presence = StableRelayPresence::for_claude_json(&path);
        for _ in 0..200 {
            assert_ne!(
                presence.check(),
                RelayVerdict::Absent,
                "concurrent atomic replacement of a relay-PRESENT config must never nag"
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
    }

    /// LATENCY (DoD #5) p50/p99 of the read+parse+fold path (instant sleeper, so
    /// the deliberate `(k-1)·T`≈50ms window is excluded — that bound is fixed by
    /// the constants). Generous budget; numbers PRINTED for the verifier.
    #[test]
    fn stable_read_latency_p50_p99() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        atomic_write(&path, PRESENT);
        let presence = StableRelayPresence::with(
            ClaudeJsonReader::new(&path),
            NoSleep,
            STABLE_READ_ATTEMPTS,
            0,
        );
        let n = 100;
        let mut us: Vec<u128> = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            let _ = presence.check();
            us.push(t.elapsed().as_micros());
        }
        us.sort_unstable();
        let p50 = us[n / 2];
        let p99 = us[(n * 99 / 100).min(n - 1)];
        println!("RelayPresence read+parse latency: p50={p50}us p99={p99}us (k={STABLE_READ_ATTEMPTS}); bounded window adds (k-1)*{STABLE_READ_SPACING_MS}ms");
        assert!(p99 < 50_000, "read+parse p99 {p99}us exceeds 50ms");
    }
}
