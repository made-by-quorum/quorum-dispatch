//! Proof that a REFUSED discovery read is carried forward as "undetermined"
//! instead of collapsing into "absent".
//!
//! ## The bug this pins
//!
//! `qd send` selects a carrier purely from one registry/join snapshot — it does
//! no probing of its own, by design. So when `gather` discarded its
//! `io::Result`s (`pt.ppid_map().unwrap_or_default()`), a `ps` refused by a
//! sandbox produced an EMPTY ancestry map, `match_by_ancestry` matched no relay,
//! and EVERY claude row silently lost its `relay_port`. `qd send` then reported
//! "it has no live receive path" — asserting an absence it had never observed,
//! with no trace of the `EPERM` anywhere in its output.
//!
//! These tests run the REAL `gather` against the frozen `home-basic` fixture and
//! pin both halves: that a denied read still nulls the field (it must — the join
//! is unchanged), and that the denial is now RECORDED alongside it, which is
//! what lets the verb layer tell the two apart.

mod common;

use dispatch::discovery::DiscoveryHealth;
use dispatch::effects::{DeniedProcessTable, FixtureProcessTable};
use dispatch::join::{self, JoinOpts};
use dispatch::mux::FixtureMux;

use common::{basic_mux, basic_process_table, empty_probe, env_with_zmx_dir, TestHome};

struct Harness {
    home: TestHome,
    tmp_root: tempfile::TempDir,
    canonical: std::path::PathBuf,
    legacy: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let home = TestHome::from_fixture("home-basic");
        home.freeze_basic_mtimes();
        let tmp_root = tempfile::tempdir().unwrap();
        let canonical = tmp_root.path().join("canonical").join("zmx-501");
        let legacy = tmp_root.path().join("legacy-ctx").join("zmx-501");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        Harness {
            home,
            tmp_root,
            canonical,
            legacy,
        }
    }

    fn gather(&self, mux: &FixtureMux, pt: &dyn dispatch::effects::ProcessTable) -> join::JoinInputs {
        let env = env_with_zmx_dir(&self.canonical);
        let probe = empty_probe();
        let clock = dispatch::effects::FixedClock(1_717_500_300_000);
        join::gather(
            &self.home.paths,
            mux,
            &env,
            pt,
            &probe,
            &clock,
            self.tmp_root.path(),
            None,
            JoinOpts::default(),
        )
    }

    fn healthy_mux(&self) -> FixtureMux {
        basic_mux(&self.canonical, &self.legacy)
    }
}

/// Baseline: reads that SUCCEED report clean health, so an empty result keeps
/// meaning "nothing there" and nothing downstream changes.
#[test]
fn a_successful_gather_reports_clean_health() {
    let h = Harness::new();
    let inputs = h.gather(&h.healthy_mux(), &basic_process_table());

    assert_eq!(
        inputs.discovery,
        DiscoveryHealth::default(),
        "every read succeeded — health must be clean"
    );
    assert!(!inputs.discovery.is_degraded());
    assert!(!inputs.discovery.receive_path_undetermined());
}

/// An EMPTY process table is not a FAILED one. A host really can have no
/// matching ancestry; that is an observation, and it must not be reported as a
/// degradation or the signal becomes worthless.
#[test]
fn an_empty_process_table_is_an_observation_not_a_degradation() {
    let h = Harness::new();
    let empty = FixtureProcessTable::default();
    let inputs = h.gather(&h.healthy_mux(), &empty);

    assert!(
        !inputs.discovery.is_degraded(),
        "a successful read that found nothing is not a degradation"
    );
}

/// THE regression. A refused `ps` is recorded as a permission denial, and the
/// evidence carries the underlying OS error forward verbatim.
#[test]
fn a_refused_process_table_is_recorded_as_a_permission_denial() {
    let h = Harness::new();
    let inputs = h.gather(&h.healthy_mux(), &DeniedProcessTable::default());

    let health = &inputs.discovery;
    assert!(health.is_degraded(), "a refused `ps` must degrade health");
    assert!(health.permission_denied(), "EPERM is a permission denial");
    assert!(
        health.receive_path_undetermined(),
        "the ancestry walk feeds relay_port — its refusal undetermines the receive path"
    );
    assert!(
        health.evidence().contains("ps failed"),
        "the evidence must name the read that failed: {}",
        health.evidence()
    );
    assert!(
        health.hint().is_some(),
        "a permission denial has an escalation remedy to offer"
    );
}

/// The other half of the same story: a refused `ps` DOES still null `relay_port`
/// (the join is deliberately unchanged), which is exactly why recording the
/// denial is load-bearing. Without health these rows are indistinguishable from
/// genuinely relay-less ones.
#[test]
fn a_refused_process_table_still_nulls_relay_port_but_no_longer_silently() {
    let h = Harness::new();

    let healthy = h.gather(&h.healthy_mux(), &basic_process_table());
    let healthy_rows = join::join_sessions(&healthy, JoinOpts::default());
    let had_relay = healthy_rows.iter().filter(|s| s.relay_port.is_some()).count();
    assert!(
        had_relay > 0,
        "fixture precondition: the healthy gather must resolve at least one relay port"
    );

    let denied = h.gather(&h.healthy_mux(), &DeniedProcessTable::default());
    let denied_rows = join::join_sessions(&denied, JoinOpts::default());
    let still_has_relay = denied_rows.iter().filter(|s| s.relay_port.is_some()).count();

    assert_eq!(
        still_has_relay, 0,
        "with no ancestry map there is no relay match — the join is unchanged"
    );
    // ...and THAT is now accompanied by the reason, which is the whole fix.
    assert!(denied.discovery.receive_path_undetermined());
}

/// A refused mux list undetermines the PTY carrier the same way the process
/// table undetermines the relay carrier.
#[test]
fn a_refused_mux_list_undetermines_the_pty_carrier() {
    let h = Harness::new();
    let denied_mux = h.healthy_mux().with_denied_list();
    let inputs = h.gather(&denied_mux, &basic_process_table());

    assert!(inputs.discovery.mux_list.is_some(), "the refusal is recorded");
    assert!(inputs.discovery.permission_denied());
    assert!(inputs.discovery.receive_path_undetermined());
    assert!(
        inputs.discovery.evidence().contains("mux list failed"),
        "evidence must name the mux read: {}",
        inputs.discovery.evidence()
    );
    assert!(
        inputs.zmx_sessions.is_empty(),
        "a failed list yields no panes — but now says why"
    );
}

/// Both reads refused: every failure is reported, not just the first one found.
#[test]
fn every_refused_read_appears_in_the_evidence() {
    let h = Harness::new();
    let inputs = h.gather(
        &h.healthy_mux().with_denied_list(),
        &DeniedProcessTable::default(),
    );

    let evidence = inputs.discovery.evidence();
    assert!(evidence.contains("ps failed"), "{evidence}");
    assert!(evidence.contains("mux list failed"), "{evidence}");
}
