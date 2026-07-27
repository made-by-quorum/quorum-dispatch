//! C-CHAOS — pi tier-(a) fault-injection loop component (WS-A.2). The thin,
//! gated wrapper over [`dispatch::provider::pi::chaos`] (the driving + at-source
//! assertions live in the library, pi house-style; this supplies the live wiring,
//! runs one round, and EMITS the evidence artifact).
//!
//! **What runs when.** The connectionless probes (class 3a daemon-gone, 3c
//! transcript-root pin, 4b setsid-escapee) ALWAYS run — they are cred-free AND
//! daemon-free, so they form the offline lane (compile-gated staging:
//! compile-check `--no-run` → offline → live). The live classes (1 reconnect,
//! 2 crash+rebind, 3b wedge, 4a pgid-teardown) run only under `QD_PI_LIVE=1`
//! (they spawn real pi residents).
//!
//! **The verdict.** A REAL SUT break (`is_break:true`) FAILS the test (panics with
//! the at-source evidence). A CHARACTERIZATION (`is_break:false`) is pinned for the
//! tier-a acceptance oracle and does NOT fail the round. The whole [`ChaosReport`]
//! is written to an inspectable JSON artifact + a per-class `RAN.txt` (the
//! live-gated-noop guard: a gate-skip writes nothing).
//!
//! Run (live, per the preregistered run-protocol):
//!   cargo build -p quorum-dispatch --bin qrmux  # build deps first
//!   QD_PI_LIVE=1 QD_PI_BIN=~/.npm-pi-global/bin/pi \
//!   QD_CHAOS_ROUND=1 \
//!   QD_CHAOS_EVIDENCE_DIR="$PWD/target/cred-evidence/cchaos/round1" \
//!   XDG_RUNTIME_DIR=/tmp/xrd \
//!     env -u QD_HOME -u QD_SESSION_ID -u SB_SESSION_ID -u QD_BOOT_AWAIT_RELAY \
//!         -u CLAUDE_CODE_SESSION_ID \
//!     cargo test -p quorum-dispatch --features faultinj --test pi_chaos -- --nocapture

#[path = "common/live_gate.rs"]
mod live_gate;

use std::path::PathBuf;

use dispatch::provider::pi::chaos::{run_chaos_round, ChaosReport};
use dispatch::provider::pi::conformance::QdRunner;

fn live() -> bool {
    live_gate::conformance_gate("QD_PI_LIVE", "pi_chaos")
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

fn round() -> u32 {
    std::env::var("QD_CHAOS_ROUND")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(1)
}

/// Evidence root: `QD_CHAOS_EVIDENCE_DIR` (the run-protocol sets it explicitly),
/// else `CARGO_TARGET_TMPDIR/cchaos-evidence/round{N}`.
fn evidence_root(round: u32) -> PathBuf {
    if let Ok(d) = std::env::var("QD_CHAOS_EVIDENCE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("cchaos-evidence")
        .join(format!("round{round}"))
}

#[test]
#[ignore = "chaos harness: spawns+kills real pi residents (group SIGKILL); run \
            explicitly under a throwaway HOME with --ignored. NEVER auto-run in a \
            default `cargo test` (box-safety: 2026-06-30 chaos-kill regression)."]
fn pi_cchaos_round() {
    let round = round();
    let evidence_root = evidence_root(round);
    std::fs::create_dir_all(&evidence_root).expect("create evidence root");
    let live = live();

    let pi = pi_bin();
    if live {
        assert!(
            pi.exists(),
            "QD_PI_LIVE=1 but pinned pi binary not found at {} (set QD_PI_BIN)",
            pi.display()
        );
    }

    // Isolated HOME (a tempdir): the registry + pi sessions live entirely inside it;
    // QdRunner truly-unsets the 5 session vars per spawn (the preregistered scrub).
    let home = tempfile::tempdir().expect("tempdir HOME");
    let cwd = home.path().to_string_lossy().into_owned();
    let qd = PathBuf::from(env!("CARGO_BIN_EXE_qd"));
    let runner = QdRunner::new(qd, pi, home.path().to_path_buf());

    let report: ChaosReport = run_chaos_round(&runner, &cwd, round, &evidence_root, live);

    // LIVE-RUN-EVIDENCE guard: write the inspectable artifact + print its path, so a
    // clean round is RECONSTRUCTABLE from observed state, never a bare pass count.
    let artifact = evidence_root.join("chaos-report.json");
    let json = report.to_evidence_json();
    let _ = std::fs::write(&artifact, &json);
    eprintln!(
        "=== pi C-CHAOS round {} ({}) evidence: {} ===",
        report.round,
        if live { "LIVE" } else { "connectionless-subset (set QD_PI_LIVE=1 for the full round)" },
        artifact.display()
    );
    eprintln!("{json}");

    // Characterizations are pinned for the acceptance oracle (informational).
    for c in report.characterizations() {
        eprintln!("PIN (characterization, is_break:false) [{}]: {}", c.class, c.detail);
    }

    // The round must have RUN something (the connectionless subset always produces
    // verdicts — guards a vacuous gate-skip "pass").
    assert!(
        !report.verdicts.is_empty(),
        "no verdicts produced — the round did not run"
    );

    // RULE: a REAL SUT break fails the round; characterizations do not.
    let breaks = report.breaks();
    if !breaks.is_empty() {
        let detail: Vec<String> = breaks
            .iter()
            .map(|b| format!("  [{}] BREAK: {} | observed:\n{}", b.class, b.detail, b.observed))
            .collect();
        panic!(
            "pi C-CHAOS round {} surfaced {} SUT break(s) — reproduce-first, then bubble:\n{}",
            report.round,
            breaks.len(),
            detail.join("\n")
        );
    }

    eprintln!(
        "pi C-CHAOS round {} CLEAN: {} verdicts, 0 breaks, {} characterization(s) pinned ({} mode).",
        report.round,
        report.verdicts.len(),
        report.characterizations().len(),
        if live { "LIVE" } else { "connectionless-subset" }
    );
}
