//! M5 / T3 — failure-injection scaffolding for the (c) crash/reconcile red-team.
//!
//! D5's named failure-injection harness did NOT land, so this is the MINIMAL one
//! (the signal is "ONE harness in the tree", not a second): a deterministic,
//! rollback-on-error + timeout-wrapped harness that exercises the REAL on-boot
//! reconciliation path (`qrmux server`'s startup sweep, server/mod.rs) against a
//! staged post-crash durable state, and observes the authoritative ledger via the
//! PRODUCTION `watch_terminal` pure-read reader (`dispatch::events::first_terminal_for`)
//! — never a forked reader.
//!
//! # Why staging the durable record IS the crash injection
//! Under the established PTY-survival fact (m1/PTY-SURVIVAL-FACT.md) the hosted
//! session DIES with the mux, and the durable spool is written per-EVENT at three
//! machinery-owned points (acceptance / countdown-start / fire-start-before-clear).
//! So a real `kill -9` of the mux at point X leaves EXACTLY the spool record for
//! phase X and nothing else — the record IS the entire post-crash state, and the
//! sender (qd) has already returned/exited (`--wait` watches, never writes). This
//! harness stages that exact record (the sole surviving copy), then boots a REAL
//! `qrmux server` whose startup runs the SAME `sweep::reconcile_spool` the daemon
//! runs after a real crash, resolving it to exactly ONE honest terminal WITHOUT a
//! live session to re-inject into. Determinism comes from the reconcile being a
//! pure function of the durable state; fidelity from booting the shipped binary.
//!
//! The T1 unit tests (`driver.rs`) prove the LIVE driver writes the countdown-start
//! snapshot with the draft; this harness consumes the record shape that wiring
//! produces (arm ii/iii carry the draft the live path would have snapshotted).
//!
//! Arms (each asserts via the pure-read ledger watcher, bounded):
//!  (i)   mux killed mid-fire → `pending-abandoned{unknown-inject-outcome}`.
//!  (ii)  mux killed mid-countdown, sender gone → ONE `seen-failed{recipient-gone}`;
//!        the draft (human's words) survived in the durable snapshot (sole copy).
//!  (iii) mux killed mid-fire post-clear pre-replay, sender gone (journal-is-sole-
//!        copy) → honest `pending-abandoned`; the draft words survive per the
//!        durable snapshot; NEVER a false `message-seen`.
//!  (iv)  restart with an unknown-inject-outcome entry → honest terminal, NO
//!        re-inject, NO delivered-that-wasn't; and a SECOND restart adds no second
//!        terminal (first-terminal-wins is stable across repeated restarts).
//!  (control) a provably-landed FireCompleted entry → a late `message-seen`, so the
//!        failed terminals above are meaningful (the reconcile CAN report success).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use dispatch::events::{first_terminal_for, is_terminal, parse_events, sha256_hex, EventRecord};
use qrmux::attended::driver::ledger_path;
use qrmux::attended::spool::{PendingRecord, Spool};
use qrmux::attended::FirePhase;

// --- binary locator (ack2_gate / m3 e2e pattern) ---------------------------

fn profile_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf()
}
fn qrmux_bin() -> PathBuf {
    let bin = profile_dir().join("qrmux");
    assert!(
        bin.exists(),
        "qrmux binary not found at {bin:?} — build it: cargo build -p qrmux --bin qrmux"
    );
    bin
}

// --- the crash arena: staged durable state + a real daemon boot ------------

/// A hermetic arena for ONE crash/reconcile arm. Owns the temp roots (QD_HOME +
/// socket dir), the staged spool, and the (single) booted daemon. `Drop` is the
/// rollback: it SIGKILLs the daemon and removes the temp tree, so a panicking
/// assertion never leaks a daemon or a directory.
struct CrashArena {
    root: PathBuf,
    qd_home: PathBuf,
    socket_dir: PathBuf,
    /// The daemon's `--session` (spool dir key) — also the ledger byname fallback.
    daemon_session: String,
    /// The record's `session` field = ledger sessionId key.
    session_id: String,
    daemon: Option<Child>,
}

impl CrashArena {
    fn establish(tag: &str) -> CrashArena {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp/qd-m5-faultinj").join(format!("{tag}-{nanos}"));
        let qd_home = root.join("qd_home");
        let socket_dir = root.join("sock");
        std::fs::create_dir_all(qd_home.join("state").join("sessions")).unwrap();
        std::fs::create_dir_all(&socket_dir).unwrap();
        CrashArena {
            root,
            qd_home,
            socket_dir,
            daemon_session: format!("faultinj-{tag}"),
            session_id: format!("sid-{tag}"),
            daemon: None,
        }
    }

    /// The authoritative ledger the daemon's boot sweep writes to (QD_HOME/state,
    /// sessionId key) — the same resolution `driver::ledger_path` gives the daemon.
    fn ledger(&self) -> PathBuf {
        ledger_path(
            &self.qd_home.join("state"),
            Some(&self.session_id),
            &self.daemon_session,
        )
    }

    /// The spool dir the daemon sweeps on boot: `<socket_dir>/pending/<session>`.
    fn spool(&self) -> Spool {
        Spool::open(self.socket_dir.join("pending").join(&self.daemon_session)).unwrap()
    }

    /// Build a base accepted record keyed to this arena (verb send:pty).
    fn record(&self, send_id: &str, text: &[u8]) -> PendingRecord {
        PendingRecord::accepted(
            send_id,
            sha256_hex(text),
            text.len() as u64,
            Some(self.session_id.clone()),
            Some(self.daemon_session.clone()),
            "send:pty",
            false,
            0,
        )
    }

    /// Stage the post-crash durable record (the injection) and return its draft for
    /// the caller to re-read as the sole surviving copy.
    fn stage(&self, rec: &PendingRecord) {
        self.spool().write(rec).unwrap();
    }

    /// Boot a REAL `qrmux server` — its startup runs the on-boot reconcile sweep on
    /// the staged spool exactly as after a real crash. Idempotent per arena; a
    /// second call boots a fresh daemon (used to prove restart-idempotence).
    fn boot_daemon(&mut self) {
        // Reap any prior daemon first (arm iv reboots).
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        let child = Command::new(qrmux_bin())
            .args([
                "server",
                "--socket-dir",
                &self.socket_dir.to_string_lossy(),
                "--session",
                &self.daemon_session,
            ])
            .env_clear()
        // Lifecycle-collapse A-3: relay readiness is DEFAULT-ON for `qd start`
        // now; these hermetic boots never write a relay sidecar, so opt out via
        // the transition alias (env "0" = explicit off; flag > env > default).
        .env("QD_BOOT_AWAIT_RELAY", "0")
            .env("HOME", &self.root) // hermetic: no real ~/.quorum touch
            .env("QD_HOME", &self.qd_home)
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn qrmux server");
        self.daemon = Some(child);
    }

    /// The FIRST terminal (7-set) for `send_id`, polled up to `budget` — the
    /// production `watch_terminal` reader (`first_terminal_for`), drop-immune and
    /// first-in-file-order. Timeout-wrap: `None` on expiry (never hangs).
    fn observe_terminal(&self, send_id: &str, budget: Duration) -> Option<EventRecord> {
        let deadline = Instant::now() + budget;
        loop {
            let body = std::fs::read_to_string(self.ledger()).unwrap_or_default();
            let recs = parse_events(&body).records;
            if let Some(t) = first_terminal_for(&recs, send_id) {
                return Some(t);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every terminal-kind line for `send_id` in the ledger (to assert EXACTLY ONE).
    fn terminal_count(&self, send_id: &str) -> usize {
        let body = std::fs::read_to_string(self.ledger()).unwrap_or_default();
        parse_events(&body)
            .records
            .into_iter()
            .filter(|r| {
                r.send_id().as_deref() == Some(send_id) && is_terminal(&r.event)
            })
            .count()
    }
}

impl Drop for CrashArena {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const BUDGET: Duration = Duration::from_secs(15);

// --- arm (i): mux killed mid-fire → unknown-inject-outcome ------------------

#[test]
fn arm_i_mux_killed_mid_fire_reconciles_to_pending_abandoned() {
    let mut a = CrashArena::establish("mid-fire");
    // Crash mid-fire: fire_started durable BEFORE the clear (inject MAY have run),
    // never completed, nothing landed.
    let mut rec = a.record("s-midfire", b"the human words");
    rec.phase = FirePhase::FireStarted;
    rec.fire_started = true;
    a.stage(&rec);

    a.boot_daemon();
    let t = a
        .observe_terminal("s-midfire", BUDGET)
        .expect("a terminal must be reconciled on boot");
    assert_eq!(t.event, "pending-abandoned", "unknown inject ⇒ pending-abandoned");
    assert_eq!(
        t.str_field("reason").as_deref(),
        Some("unknown-inject-outcome"),
        "honest unknown-inject reason"
    );
    assert_eq!(a.terminal_count("s-midfire"), 1, "exactly one terminal");
}

// --- arm (ii): mux killed mid-countdown, sender gone ------------------------

#[test]
fn arm_ii_mid_countdown_sender_gone_one_seen_failed_draft_preserved() {
    let mut a = CrashArena::establish("mid-countdown");
    let words = b"a half-typed reply the human never sent".to_vec();
    // Crash mid-countdown: fire NEVER started; the countdown-start snapshot (T1)
    // captured the draft into the durable record — the SOLE surviving copy.
    let mut rec = a.record("s-countdown", &words);
    rec.phase = FirePhase::Countdown;
    rec.draft = words.clone();
    a.stage(&rec);

    // The words survived the crash in the durable snapshot (sole copy) BEFORE any
    // reconcile touches it.
    let staged = a.spool().load("s-countdown").unwrap().unwrap();
    assert_eq!(staged.draft, words, "draft preserved byte-exact in the durable snapshot");

    a.boot_daemon();
    let t = a
        .observe_terminal("s-countdown", BUDGET)
        .expect("a terminal must be reconciled on boot");
    // Fire never ran + session dies with the mux ⇒ honest recipient-gone, never a
    // false landed, never a blind re-inject.
    assert_eq!(t.event, "seen-failed", "inject-not-run + session-gone ⇒ seen-failed");
    assert_eq!(t.str_field("reason").as_deref(), Some("recipient-gone"));
    assert_eq!(a.terminal_count("s-countdown"), 1, "exactly one terminal");
}

// --- arm (iii): mid-fire post-clear pre-replay, journal-is-sole-copy --------

#[test]
fn arm_iii_mid_fire_post_clear_pre_replay_words_survive() {
    let mut a = CrashArena::establish("post-clear");
    let words = b"words only in the journal snapshot".to_vec();
    // Crash after the clear ran and inject MAY have run, but BEFORE the draft replay
    // (step 6) — so the human's draft lives ONLY in the fire-start durable snapshot.
    let mut rec = a.record("s-postclear", &words);
    rec.phase = FirePhase::FireStarted;
    rec.fire_started = true;
    rec.draft = words.clone();
    a.stage(&rec);

    // Journal-is-sole-copy: the words survive per the durable snapshot.
    let staged = a.spool().load("s-postclear").unwrap().unwrap();
    assert_eq!(staged.draft, words, "words survive per the durable snapshot");

    a.boot_daemon();
    let t = a
        .observe_terminal("s-postclear", BUDGET)
        .expect("a terminal must be reconciled on boot");
    // Inject outcome unknown ⇒ honest pending-abandoned; NEVER a fabricated landed.
    assert_eq!(t.event, "pending-abandoned");
    assert_eq!(t.str_field("reason").as_deref(), Some("unknown-inject-outcome"));
    assert_ne!(t.event, "message-seen", "no delivered-that-wasn't");
    assert_eq!(a.terminal_count("s-postclear"), 1, "exactly one terminal");
}

// --- arm (iv): unknown-inject entry, no re-inject, restart-idempotent -------

#[test]
fn arm_iv_unknown_inject_no_reinject_and_restart_is_idempotent() {
    let mut a = CrashArena::establish("unknown-inject");
    let mut rec = a.record("s-unknown", b"payload");
    rec.phase = FirePhase::FireStarted;
    rec.fire_started = true;
    a.stage(&rec);

    a.boot_daemon();
    let t = a
        .observe_terminal("s-unknown", BUDGET)
        .expect("a terminal must be reconciled on boot");
    assert_eq!(t.event, "pending-abandoned");
    // NO delivered-that-wasn't: the ONLY terminal is the honest abandonment.
    let body = std::fs::read_to_string(a.ledger()).unwrap_or_default();
    assert!(
        !body.contains("\"event\":\"message-seen\""),
        "an unknown inject must NEVER produce a message-seen: {body}"
    );
    assert_eq!(a.terminal_count("s-unknown"), 1, "exactly one terminal after first restart");

    // A SECOND restart (repeated crash-recovery) must NOT re-inject nor add a second
    // terminal — first-terminal-wins is stable, so the harness is repeatable.
    a.boot_daemon();
    // Give the second daemon time to run its sweep (and observe the existing
    // terminal → idempotent skip).
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        a.terminal_count("s-unknown"),
        1,
        "second restart adds no second terminal (first-terminal-wins)"
    );
}

// --- control: a provably-landed entry → late message-seen -------------------
// Guards against a hollow suite where every arm is pending-abandoned/seen-failed
// regardless of correctness: proves the SAME boot-reconcile CAN observe a real
// landing and report success.

#[test]
fn control_fire_completed_landed_reconciles_to_message_seen() {
    let mut a = CrashArena::establish("landed");
    let landed_text = b"HELLO_M5_LANDED_CONTROL";
    // Stage a transcript whose user record carries the landed text (the default
    // TranscriptLandingProbe the daemon uses matches by content sha).
    let transcript = a.root.join("convo.jsonl");
    std::fs::write(
        &transcript,
        b"{\"type\":\"user\",\"message\":{\"content\":\"HELLO_M5_LANDED_CONTROL\"}}\n",
    )
    .unwrap();

    let mut rec = a.record("s-landed", landed_text);
    rec.phase = FirePhase::FireCompleted;
    rec.fire_started = true;
    rec.fire_completed = true;
    rec.transcript = Some(transcript.to_string_lossy().into_owned());
    rec.transcript_offset = Some(0);
    a.stage(&rec);

    a.boot_daemon();
    let t = a
        .observe_terminal("s-landed", BUDGET)
        .expect("a terminal must be reconciled on boot");
    assert_eq!(t.event, "message-seen", "a provably-landed send reconciles to message-seen");
    assert_eq!(a.terminal_count("s-landed"), 1, "exactly one terminal");
}
