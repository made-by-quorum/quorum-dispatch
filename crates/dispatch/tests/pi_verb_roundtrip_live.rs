//! Item 7 — pi tier-(a) LIVE conformance: drive the REAL `qd` verbs BY NAME against
//! a live `pi --mode rpc` for the 8 credential-free rubric items (#1,2,3,8,9,10,11,12),
//! RUN-not-read. The driving + assertions live in
//! [`dispatch::provider::pi::conformance`]; this wrapper supplies the live wiring,
//! runs the sweep, and EMITS the evidence artifact (the LIVE-RUN-EVIDENCE guard:
//! a green is proven by the inspectable report, never inferred from a pass count).
//!
//! CRED-FREE: tier-a touches no model turn, so this needs only the pinned pi 0.80.2
//! (via `QD_PI_BIN`) — NO OAuth. Gated `QD_PI_LIVE=1` (spawns real residents + pi
//! children). Run:
//!   QD_PI_LIVE=1 QD_PI_BIN=~/.npm-pi-global/bin/pi \
//!     env -u QD_HOME -u QD_SESSION_ID -u SB_SESSION_ID -u QD_BOOT_AWAIT_RELAY \
//!         -u CLAUDE_CODE_SESSION_ID \
//!     cargo test -p quorum-dispatch --test pi_verb_roundtrip_live -- --nocapture
//!
//! The real-on-disk-dir `encode_cwd_dir` confirm (needs an actual pi-created dir =
//! a turn = assistant-gated lazy-write) is TIER-B; here the CRED-FREE regex+PA5
//! shape assertion runs (deferred real-dir confirm noted in the report).

use std::path::PathBuf;

use dispatch::provider::pi::conformance::{run_tier_a, QdRunner};

fn live() -> bool {
    std::env::var("QD_PI_LIVE").as_deref() == Ok("1")
}

/// The pinned pi binary: `QD_PI_BIN` if set, else the quorum-box default
/// (`~/.npm-pi-global/bin/pi`). pi is NOT on PATH.
fn pi_bin() -> PathBuf {
    if let Ok(b) = std::env::var("QD_PI_BIN") {
        if !b.is_empty() {
            return PathBuf::from(b);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".npm-pi-global/bin/pi")
}

/// The installed `rpc-types.d.ts` for the #12 shape/hash pin: `QD_PI_DTS` if set,
/// else the npm-global install path.
fn d_ts_path() -> PathBuf {
    if let Ok(p) = std::env::var("QD_PI_DTS") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(
        ".npm-pi-global/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/rpc/rpc-types.d.ts",
    )
}

#[test]
fn pi_tier_a_conformance_live() {
    if !live() {
        eprintln!("pi_tier_a_conformance_live: SKIPPED (set QD_PI_LIVE=1 to run the live sweep)");
        return;
    }
    let pi = pi_bin();
    assert!(
        pi.exists(),
        "pinned pi binary not found at {} (set QD_PI_BIN)",
        pi.display()
    );
    let dts = d_ts_path();
    let qd = PathBuf::from(env!("CARGO_BIN_EXE_qd"));

    // Isolated HOME (a tempdir) → the registry + pi sessions live entirely inside it;
    // QdRunner also truly-unsets the 5 session vars per spawn (the preregistered scrub).
    let home = tempfile::tempdir().expect("tempdir HOME");
    let runner = QdRunner::new(qd, pi, home.path().to_path_buf());

    let report = run_tier_a(&runner, Some(&dts));

    // LIVE-RUN-EVIDENCE guard: write the inspectable artifact + print its path, so a
    // green is RECONSTRUCTABLE from observed state, never a bare pass count.
    let evidence_path = std::env::var("QD_PI_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.path().join("pi-tier-a-evidence.json"));
    let json = report.to_evidence_json();
    let _ = std::fs::write(&evidence_path, &json);
    eprintln!("=== pi tier-(a) conformance evidence: {} ===", evidence_path.display());
    eprintln!("{json}");

    // Rule on the report.
    if !report.all_green() {
        let fails: Vec<String> = report
            .failures()
            .iter()
            .map(|r| format!("  #{} FAIL: {} | observed: {}", r.item, r.detail, r.observed))
            .collect();
        panic!(
            "pi tier-(a) conformance NOT green ({} item-results failed):\n{}",
            report.failures().len(),
            fails.join("\n")
        );
    }
    eprintln!(
        "pi tier-(a) conformance GREEN: {} item-results all passed (8 rubric items).",
        report.items.len()
    );
}
