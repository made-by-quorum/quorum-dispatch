//! (c) The flake-attribution exercise (M-3) — EXERCISE EVIDENCE, NOT RELEASE.
//!
//! C-4 (c): exercise the flake-attribution discipline deterministically, incl.
//! its negative space, driven through the EXACT production accounting path —
//! [`super::attribution::resolve_heads`] (chain/head resolution) and
//! [`super::tier::compute_tier`] STAGE-5 (A6 pending / A8 authority pooling).
//! Constructed corpora only; nothing here is published as release evidence.
//!
//! A non-gating cell (`d5.n`) FAILS once (the "flake") in the middle run of a
//! W=3 window; the disposition of that fail's attribution then governs the rate:
//!   - HARNESS-attributed  → the fail leaves BOTH numerator and denominator
//!     (FailHarnessRemoved) → the pooled rate rises.
//!   - PRODUCT-attributed   → the fail stays in the denominator (the demotion
//!     stands) → rate unchanged from the no-record baseline.
//!   - no record / malformed / barred issuer → FailPending → pool-only,
//!     IDENTICAL rate to the product/no-record case (assert the RATE, not the
//!     class string — n1/n2/n3 surface `FailPending`, not `FailProduct`).
//!   - second-root / fork  → the pre-existing valid head continues to govern.
//!   - decoy run (attribution.run absent from the journal) → the whole corpus
//!     is `INVALID_EVIDENCE` (v9 attribution-run binding), never a quiet rate.
//! Every case carries a POSITIVE CONTROL that its intended site actually fired
//! (well_formed rejects / the record appears in `ChainResolution.invalid` / a
//! differential shows the authority filter is what rejected it / the verdict is
//! `InvalidEvidence`) — a case that green-passes without exercising its target
//! is a defective instrument, void not clean.
//!
//! Run-kind note: the synthetic window runs are `RunKind::Evidence` — a run must
//! enter a window for the accounting to be observable at all (an `Exercise`-kind
//! run is foreclosed from every window by construction). The "exercise, not
//! release" discipline is met structurally here: these corpora are in-test
//! fixtures, serialized and published nowhere.

use std::collections::BTreeMap;

use super::attribution::{resolve_heads, AttributionRecord, Disposition, ReasonCode};
use super::evidence::{CommissioningHeader, RunArtifactBuilder, RunMode};
use super::ids::{
    AggregationVersion, AttributionSeq, BoxId, CellId, Lane, LaneScope, ObservationId, RecordId,
    RunId, RunKind,
};
use super::journal::{
    AuthorityJournal, ContextActivationKind, DesignationBasisKind, DesignationEvent,
    DesignationKind, TerminalState,
};
use super::registry::{Applicability, Battery, Cell, Dimension};
use super::tier::{
    compute_tier, ArtifactIndex, CommitContext, Corpus, IndexedArtifact, RateValue, RoleRegistry,
    TierParams, TierVerdict,
};

const COMMIT: &str = "commit-r9";
const BOX: &str = "lima";
const SNAP: &str = "2026-07-15T12:00:00Z";
const FLAKE_RUN: &str = "run-2";
const FLAKE_CELL: &str = "d5.n";
/// A neutral attribution seat — NOT the run's coordinator/runner/lane-owner/cell
/// author, so A8 authority accepts it.
const NEUTRAL: &str = "attribution-seat (9999zzzz)";
/// The run's own coordinator — A8 BARS it from ruling its own run's attribution.
const BARRED_COORDINATOR: &str = "coord-1 (aaaa1111)";

/// A tiny battery: two gating (d1.g/d6.g) + two non-gating (d3.n/d5.n) cells on
/// Pi, so a full grid stays small and the flake lands on a non-gating cell.
fn base_battery() -> Battery {
    let cells = vec![
        Cell {
            id: CellId("d1.g".into()),
            dimension: Dimension::D1Lifecycle,
            title: "gating d1".into(),
        },
        Cell {
            id: CellId("d6.g".into()),
            dimension: Dimension::D6FailureHonesty,
            title: "gating d6".into(),
        },
        Cell {
            id: CellId("d3.n".into()),
            dimension: Dimension::D3BusyQueue,
            title: "non-gating d3".into(),
        },
        Cell {
            id: CellId("d5.n".into()),
            dimension: Dimension::D5Transcript,
            title: "non-gating d5".into(),
        },
    ];
    let mut applicability = BTreeMap::new();
    for c in ["d1.g", "d6.g", "d3.n", "d5.n"] {
        applicability.insert(
            (Lane::Pi.provider_id().to_string(), c.to_string()),
            Applicability::Required,
        );
    }
    Battery {
        cells,
        applicability,
        aggregation_version: AggregationVersion("agg-v1".into()),
    }
}

/// The flake fail's observation id (run-2, Pi, d5.n).
fn flake_obs() -> ObservationId {
    ObservationId::derive(
        &RunId(FLAKE_RUN.into()),
        Lane::Pi,
        &CellId(FLAKE_CELL.into()),
    )
}

/// Build a valid corpus: W=3 Evidence runs on the designated box, all cells Pass
/// except the flake (d5.n FAILS once, in run-2). Attribution records + roles are
/// supplied by the caller so each case varies only the attribution.
fn build_corpus(attributions: Vec<AttributionRecord>, roles: RoleRegistry) -> Corpus {
    let battery = base_battery();
    let mut journal = AuthorityJournal::new();
    journal.activate_context(ContextActivationKind::Manifest(battery.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(
        battery.aggregation_version.clone(),
    ));
    journal.designate(DesignationEvent {
        kind: DesignationKind::Designate,
        lane: Lane::Pi,
        box_id: BoxId(BOX.into()),
        basis_kind: Some(DesignationBasisKind::GreaterRunnability),
        policy_basis: "greater applicable-cell runnability at representative shape".into(),
        rationale: "representative designated box for the exercise".into(),
        authorized_by: "designation-seat (dddd4444)".into(),
        timestamp: "2026-07-13T00:00:00Z".into(),
    });

    let mut index = Vec::new();
    let mut tok = 0u64;
    let mut nonce = 0u64;
    for run in ["run-1", "run-2", "run-3"] {
        tok += 1;
        nonce += 1;
        let tuple = journal
            .commission_run(
                RunId(run.into()),
                LaneScope::one(Lane::Pi),
                BoxId(BOX.into()),
                COMMIT,
                battery.manifest_digest(),
                battery.aggregation_version.clone(),
                RunKind::Evidence,
                BARRED_COORDINATOR,
                &mut || format!("tok-{tok}"),
            )
            .unwrap();
        let launch = journal
            .start_run_at(
                &tuple.run,
                "runner-1 (bbbb2222)",
                "2026-07-14T00:00:00Z",
                &mut || format!("nonce-{nonce}"),
            )
            .unwrap();
        let header = CommissioningHeader::new(tuple, launch);
        let mut b = RunArtifactBuilder::new(header, "2026-07-14T00:00:00Z");
        let mut applicable = Vec::new();
        for cell in &battery.cells {
            let cid = cell.id.clone();
            let outcome = if run == FLAKE_RUN && cid.0 == FLAKE_CELL {
                b.runner().fail(
                    vec!["cmd".into()],
                    "observed a flake fail",
                    "the flake fired this round",
                )
            } else {
                b.runner().pass(vec!["cmd".into()], "observed state")
            };
            b.observe(Lane::Pi, cid.clone(), RunMode::Automated, outcome);
            applicable.push((Lane::Pi, cid));
        }
        let artifact = b.build(&applicable).unwrap();
        let digest = artifact.content_digest();
        journal
            .mark_terminal(
                &RunId(run.into()),
                TerminalState::Completed {
                    artifact_digest: digest.clone(),
                },
            )
            .unwrap();
        index.push(IndexedArtifact { digest, artifact });
    }
    let head = ArtifactIndex {
        artifacts: index.clone(),
    }
    .head_digest();
    journal.publish_snapshot(head, SNAP);

    let mut commits = BTreeMap::new();
    commits.insert(COMMIT.to_string(), CommitContext { battery });
    Corpus {
        journal,
        index: ArtifactIndex { artifacts: index },
        attributions,
        commits,
        roles,
    }
}

/// A well-formed attribution record over the flake observation.
fn record(
    id: &str,
    seq: u64,
    disposition: Disposition,
    reason: ReasonCode,
    issuer: &str,
    run: &str,
    supersedes: Vec<&str>,
) -> AttributionRecord {
    AttributionRecord {
        id: RecordId(id.into()),
        sequence: AttributionSeq::new(seq),
        run: RunId(run.into()),
        observations: vec![flake_obs()],
        disposition,
        reason,
        primary_evidence: vec!["evidence-ref-1".into()],
        issuer: issuer.into(),
        timestamp: "2026-07-15T00:00:00Z".into(),
        supersedes: supersedes.into_iter().map(|s| RecordId(s.into())).collect(),
    }
}

/// The pooled rate for a corpus carrying `attributions` (neutral roles). Panics
/// if the corpus does not compute a rate (i.e. only for the valid cases).
fn rate_of(attributions: Vec<AttributionRecord>) -> RateValue {
    let comp = compute_tier(
        &build_corpus(attributions, RoleRegistry::default()),
        Lane::Pi,
        COMMIT,
        &TierParams::gated(),
    );
    comp.rate.expect("a valid corpus computes a pooled rate")
}

fn harness_record(id: &str, seq: u64, issuer: &str, supersedes: Vec<&str>) -> AttributionRecord {
    record(
        id,
        seq,
        Disposition::Harness,
        ReasonCode::HarnessFlakeTiming,
        issuer,
        FLAKE_RUN,
        supersedes,
    )
}
fn product_record(id: &str, seq: u64, issuer: &str, supersedes: Vec<&str>) -> AttributionRecord {
    record(
        id,
        seq,
        Disposition::Product,
        ReasonCode::ProductDefect,
        issuer,
        FLAKE_RUN,
        supersedes,
    )
}

// ---- POSITIVE dispositions ------------------------------------------------

#[test]
fn positive_harness_attributed_fail_leaves_the_rate() {
    let base = rate_of(vec![]); // pending baseline
    let harness = rate_of(vec![harness_record("rec-h", 1, NEUTRAL, vec![])]);
    match (base, harness) {
        (
            RateValue::Rate {
                passes: bp,
                pool: bpool,
            },
            RateValue::Rate {
                passes: hp,
                pool: hpool,
            },
        ) => {
            assert_eq!(hp, bp, "harness: numerator unchanged");
            assert_eq!(
                hpool,
                bpool - 1,
                "harness: the flake fail is removed from the denominator (fail leaves the rate)"
            );
        }
        other => panic!("expected pooled rates, got {other:?}"),
    }
    // Positive control: the harness record actually resolved as the active head.
    let res = resolve_heads(&[harness_record("rec-h", 1, NEUTRAL, vec![])]);
    assert_eq!(
        res.head_of(&flake_obs()).map(|r| r.0.as_str()),
        Some("rec-h"),
        "the harness record is the active head"
    );
    assert!(
        res.invalid.is_empty(),
        "a lone well-formed record is not invalid"
    );
}

#[test]
fn positive_product_attributed_demotion_stands() {
    let base = rate_of(vec![]);
    let product = rate_of(vec![product_record("rec-p", 1, NEUTRAL, vec![])]);
    assert_eq!(product, base, "product: the fail stays in the denominator — the demotion stands (rate == pending baseline)");
    let harness = rate_of(vec![harness_record("rec-h", 1, NEUTRAL, vec![])]);
    assert_ne!(product, harness, "product keeps the fail while harness removes it — the two dispositions are distinguishable in the rate");
}

// ---- NEGATIVE space -------------------------------------------------------

#[test]
fn n1_no_record_stays_product_counted() {
    let base = rate_of(vec![]); // n1 IS the no-record case
    let product = rate_of(vec![product_record("rec-p", 1, NEUTRAL, vec![])]);
    assert_eq!(
        base, product,
        "n1 no-record rate == product-attributed rate (the fail is pool-counted either way)"
    );
    // Positive control: the corpus computes with zero attributions present.
    let comp = compute_tier(
        &build_corpus(vec![], RoleRegistry::default()),
        Lane::Pi,
        COMMIT,
        &TierParams::gated(),
    );
    assert!(
        comp.rate.is_some(),
        "the no-record corpus still computes a rate (the fail is pending)"
    );
    assert!(
        comp.verdict.tier() != super::tier::Tier::Ga,
        "a pending flake fail keeps the lane below GA"
    );
}

#[test]
fn n2_malformed_record_rejected_stays_product_counted() {
    let base = rate_of(vec![]);
    let mut malformed = harness_record("rec-bad", 1, NEUTRAL, vec![]);
    malformed.primary_evidence = vec![]; // malformed: no primary-evidence refs
                                         // Positive control: well_formed rejects it AT ITS SITE.
    assert!(
        malformed.well_formed().is_err(),
        "the malformed record is rejected by well_formed"
    );
    // The malformed record does NOT harness-remove the fail → rate == no-record.
    assert_eq!(
        rate_of(vec![malformed]),
        base,
        "a malformed record leaves the fail product-counted (rate == no-record)"
    );
    // Differential control: a WELL-FORMED version DOES change the rate — proving
    // it was malformedness, not general non-application, that rejected it.
    assert_ne!(
        rate_of(vec![harness_record("rec-good", 1, NEUTRAL, vec![])]),
        base,
        "a well-formed harness record changes the rate (differential)"
    );
}

#[test]
fn n3_barred_identity_rejected_stays_product_counted() {
    let base = rate_of(vec![]);
    // The run's own coordinator is barred from ruling its attribution (A8).
    let barred = harness_record("rec-barred", 1, BARRED_COORDINATOR, vec![]);
    assert_eq!(
        rate_of(vec![barred]),
        base,
        "a barred-issuer record leaves the fail product-counted (rate == no-record)"
    );
    // Differential control: the SAME record from a neutral seat DOES change the
    // rate — so it was the AUTHORITY filter, not the record's form, that rejected it.
    assert_ne!(rate_of(vec![harness_record("rec-neutral", 1, NEUTRAL, vec![])]), base, "a neutral-issuer record changes the rate (the authority filter is what rejected the barred one)");
}

#[test]
fn n4_second_root_preexisting_head_governs() {
    // rec-1 (harness) roots the chain; rec-2 is a SECOND ROOT over the same
    // already-chained observation without superseding rec-1.
    let rec1 = harness_record("rec-1", 1, NEUTRAL, vec![]);
    let rec2_second_root = product_record("rec-2", 2, NEUTRAL, vec![]); // supersedes NOTHING → second root
                                                                        // Positive control: resolve_heads flags rec-2 invalid and keeps rec-1 as head.
    let res = resolve_heads(&[rec1.clone(), rec2_second_root.clone()]);
    assert_eq!(
        res.head_of(&flake_obs()).map(|r| r.0.as_str()),
        Some("rec-1"),
        "the pre-existing head rec-1 continues to govern"
    );
    assert!(
        res.invalid.iter().any(|iv| iv.id.0 == "rec-2"),
        "the second-root rec-2 is flagged invalid, never silently honored"
    );
    // Accounting: the pre-existing harness head governs → rate == harness-only,
    // NOT flipped to product by the invalid second root.
    let harness_only = rate_of(vec![harness_record("rec-1", 1, NEUTRAL, vec![])]);
    assert_eq!(
        rate_of(vec![rec1, rec2_second_root]),
        harness_only,
        "the head-only (harness) rate governs; the second root does not flip it to product"
    );
}

#[test]
fn n5_fork_first_sibling_governs() {
    // rec-0 roots; rec-a and rec-b BOTH supersede rec-0 (siblings) — a fork,
    // one harness one product. rec-a (lower precedence) applies; rec-b then
    // supersedes a record that is no longer the head → fork → invalid.
    let rec0 = harness_record("rec-0", 1, NEUTRAL, vec![]);
    let rec_a = harness_record("rec-a", 2, NEUTRAL, vec!["rec-0"]);
    let rec_b = product_record("rec-b", 3, NEUTRAL, vec!["rec-0"]);
    let res = resolve_heads(&[rec0.clone(), rec_a.clone(), rec_b.clone()]);
    assert_eq!(
        res.head_of(&flake_obs()).map(|r| r.0.as_str()),
        Some("rec-a"),
        "the first sibling rec-a governs"
    );
    assert!(
        res.invalid.iter().any(|iv| iv.id.0 == "rec-b"),
        "the fork sibling rec-b is flagged invalid, never silently honored"
    );
    // Accounting: rec-a (harness) governs → the fail is removed, not demoted.
    let harness_only = rate_of(vec![harness_record("rec-x", 1, NEUTRAL, vec![])]);
    assert_eq!(
        rate_of(vec![rec0, rec_a, rec_b]),
        harness_only,
        "the governing harness sibling's rate stands; the fork does not flip it to product"
    );
}

#[test]
fn c2_decoy_run_yields_invalid_evidence() {
    // A record whose run ("run-DECOY") is absent from the journal, but whose
    // observation belongs to run-2 — the v9 attribution-run binding fails the
    // WHOLE corpus, never quietly yielding a pending/product rate.
    let decoy = record(
        "rec-decoy",
        1,
        Disposition::Harness,
        ReasonCode::HarnessFlakeTiming,
        NEUTRAL,
        "run-DECOY",
        vec![],
    );
    let comp = compute_tier(
        &build_corpus(vec![decoy], RoleRegistry::default()),
        Lane::Pi,
        COMMIT,
        &TierParams::gated(),
    );
    // Positive control: the verdict is INVALID_EVIDENCE (v9), not a rate.
    match &comp.verdict {
        TierVerdict::InvalidEvidence { failed_checks } => {
            assert!(
                failed_checks.iter().any(|c| c.contains("v9") || c.to_lowercase().contains("attribution")),
                "the invalid-evidence reason names the attribution-run binding (v9): {failed_checks:?}"
            );
        }
        other => panic!("decoy-run attribution must yield INVALID_EVIDENCE, got {other:?}"),
    }
    assert!(
        comp.rate.is_none(),
        "an INVALID_EVIDENCE corpus carries no pooled rate"
    );
}
