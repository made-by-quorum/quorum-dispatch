//! (d) The governance exercise (N-1/N-2) — EXERCISE EVIDENCE, NOT RELEASE.
//!
//! C-4 (d), reading (B) hybrid: the 22 governance cases are covered by a SPLIT —
//! ~9 by C-3's `tier_tests` (cited in the coverage map, verified to drive the
//! production path and assert the intended check), and the GAPS + honest-twins +
//! the Exercise-kind FORECLOSURE case + the positive path authored HERE. This
//! module is additive: it drives the EXACT production validators
//! (`AuthorityJournal::integrity_check` via `compute_tier`'s v1, the corpus-scope
//! v2..v9, designation replay, and `tier_doc::publish_tier_document`) over
//! constructed journals/corpora — NO reimplementation, NO change to `tier_tests`.
//!
//! Each case carries a POSITIVE CONTROL that its intended check actually fired
//! (an `INVALID_EVIDENCE` whose `failed_checks` names THIS case's check, an
//! append-time API rejection, or — for the honest-twins — that the honest corpus
//! COMPUTES normally, proving the check discriminates rather than blanket-rejects).
//! Fixtures are in-test, serialized/published nowhere (exercise-not-release); the
//! synthetic runs are `RunKind::Evidence` so they enter a window (an Exercise run
//! is foreclosed — the FORECLOSURE case proves exactly that).

use std::collections::BTreeMap;

use super::evidence::{CommissioningHeader, RunArtifactBuilder, RunMode};
use super::ids::{
    AggregationVersion, BoxId, CellId, Lane, LaneScope, RunId, RunKind,
};
use super::journal::{
    AuthorityJournal, ContextActivationKind, DesignationBasisKind, DesignationEvent,
    DesignationKind, TerminalState,
};
use super::registry::{Applicability, Battery, Cell, Dimension};
use super::tier::{
    compute_tier, ArtifactIndex, CommitContext, Corpus, IndexedArtifact, RoleRegistry, TierParams,
    TierVerdict,
};

const COMMIT: &str = "commit-r9";
const BOX: &str = "lima";
const SNAP: &str = "2026-07-15T12:00:00Z";
const COORD: &str = "coord-1 (aaaa1111)";
const RUNNER: &str = "runner-1 (bbbb2222)";

fn base_battery() -> Battery {
    let cells = vec![
        Cell { id: CellId("d1.g".into()), dimension: Dimension::D1Lifecycle, title: "gating d1".into() },
        Cell { id: CellId("d6.g".into()), dimension: Dimension::D6FailureHonesty, title: "gating d6".into() },
        Cell { id: CellId("d3.n".into()), dimension: Dimension::D3BusyQueue, title: "non-gating d3".into() },
        Cell { id: CellId("d5.n".into()), dimension: Dimension::D5Transcript, title: "non-gating d5".into() },
    ];
    let mut applicability = BTreeMap::new();
    for c in ["d1.g", "d6.g", "d3.n", "d5.n"] {
        applicability.insert((Lane::Pi.provider_id().to_string(), c.to_string()), Applicability::Required);
    }
    Battery { cells, applicability, aggregation_version: AggregationVersion("agg-v1".into()) }
}

/// A clean W=3 all-Pass corpus on the designated box → GA. The base every
/// negative case mutates and the positive path asserts unchanged.
fn ga_corpus() -> Corpus {
    let battery = base_battery();
    let mut journal = AuthorityJournal::new();
    journal.activate_context(ContextActivationKind::Manifest(battery.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(battery.aggregation_version.clone()));
    journal.designate(DesignationEvent {
        kind: DesignationKind::Designate,
        lane: Lane::Pi,
        box_id: BoxId(BOX.into()),
        basis_kind: Some(DesignationBasisKind::GreaterRunnability),
        policy_basis: "greater applicable-cell runnability at representative shape".into(),
        rationale: "representative designated box".into(),
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
                COORD,
                &mut || format!("tok-{tok}"),
            )
            .unwrap();
        let launch = journal
            .start_run_at(&tuple.run, RUNNER, "2026-07-14T00:00:00Z", &mut || format!("nonce-{nonce}"))
            .unwrap();
        let header = CommissioningHeader::new(tuple, launch);
        let mut b = RunArtifactBuilder::new(header, "2026-07-14T00:00:00Z");
        let mut applicable = Vec::new();
        for cell in &battery.cells {
            let cid = cell.id.clone();
            b.observe(Lane::Pi, cid.clone(), RunMode::Automated, b.runner().pass(vec!["cmd".into()], "observed state"));
            applicable.push((Lane::Pi, cid));
        }
        let artifact = b.build(&applicable).unwrap();
        let digest = artifact.content_digest();
        journal
            .mark_terminal(&RunId(run.into()), TerminalState::Completed { artifact_digest: digest.clone() })
            .unwrap();
        index.push(IndexedArtifact { digest, artifact });
    }
    let head = ArtifactIndex { artifacts: index.clone() }.head_digest();
    journal.publish_snapshot(head, SNAP);
    let mut commits = BTreeMap::new();
    commits.insert(COMMIT.to_string(), CommitContext { battery });
    Corpus { journal, index: ArtifactIndex { artifacts: index }, attributions: Vec::new(), commits, roles: RoleRegistry::default() }
}

/// Re-serialize the journal, mutate it as raw JSON, deserialize back — the way
/// to construct states the typed API forbids (forged ordinals, dup ids, …).
fn mutate_journal(corpus: &mut Corpus, f: impl FnOnce(&mut serde_json::Value)) {
    let mut v = serde_json::to_value(&corpus.journal).unwrap();
    f(&mut v);
    corpus.journal = serde_json::from_value(v).unwrap();
}

/// Re-anchor the snapshot to the current index head (after mutating the index).
fn reanchor(corpus: &mut Corpus) {
    mutate_journal(corpus, |v| {
        v["entries"].as_array_mut().unwrap().retain(|e| e["body"]["event"] != "publication-snapshot");
    });
    let head = corpus.index.head_digest();
    corpus.journal.publish_snapshot(head, SNAP);
}

#[track_caller]
fn assert_invalid(corpus: &Corpus, substr: &str) {
    let comp = compute_tier(corpus, Lane::Pi, COMMIT, &TierParams::gated());
    match &comp.verdict {
        TierVerdict::InvalidEvidence { failed_checks } => assert!(
            failed_checks.iter().any(|s| s.contains(substr)),
            "expected a failed check containing {substr:?}, got {failed_checks:#?}"
        ),
        other => panic!("expected INVALID_EVIDENCE containing {substr:?}, got {other:?}"),
    }
}

/// A positive-control for the honest-twins: the honest corpus COMPUTES a real
/// tier (never INVALID_EVIDENCE) — proving the check discriminates.
#[track_caller]
fn assert_computes(corpus: &Corpus) {
    let comp = compute_tier(corpus, Lane::Pi, COMMIT, &TierParams::gated());
    assert!(
        !matches!(comp.verdict, TierVerdict::InvalidEvidence { .. }),
        "honest corpus must compute a real tier, got {:?}",
        comp.verdict
    );
}

// ---- POSITIVE path --------------------------------------------------------

#[test]
fn positive_wellformed_corpus_computes_ga() {
    let comp = compute_tier(&ga_corpus(), Lane::Pi, COMMIT, &TierParams::gated());
    assert_eq!(comp.verdict, TierVerdict::Ga, "a well-formed all-Pass W=3 corpus computes GA");
}

// ---- REGISTRATION gaps ----------------------------------------------------

#[test]
fn r1_post_hoc_registration_rejected() {
    // run-3's run-start ordinal forged to precede its commissioning ordinal.
    let mut c = ga_corpus();
    mutate_journal(&mut c, |v| {
        // Find run-3's commission ordinal, then set its start ordinal below it.
        let mut commission_ord = None;
        for e in v["entries"].as_array().unwrap() {
            if e["body"]["event"] == "run" && e["body"]["run"] == "run-3" {
                commission_ord = e["ordinal"].as_u64();
            }
        }
        let target = commission_ord.unwrap();
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "start" && e["body"]["run"] == "run-3" {
                e["ordinal"] = serde_json::json!(target); // == commission (not strictly after)
            }
        }
    });
    assert_invalid(&c, "not strictly after its commissioning ordinal");
}

#[test]
fn r2_journal_duplicate_run_id_rejected() {
    // A journal-level duplicate run id (distinct from index-artifact duplication).
    let mut c = ga_corpus();
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "run" && e["body"]["run"] == "run-2" {
                e["body"]["run"] = serde_json::json!("run-1"); // two "run-1" run entries
            }
        }
    });
    assert_invalid(&c, "duplicate run id");
}

#[test]
fn r3_register_only_completed_below_cutoff_rejected() {
    // A completed entry at/below the cutoff whose artifact is omitted from the
    // index → terminal completeness fails (never a quietly smaller window).
    let mut c = ga_corpus();
    // Drop run-1's artifact from the index; its completion still cites the digest.
    c.index.artifacts.retain(|a| a.artifact.header.tuple.run.0 != "run-1");
    reanchor(&mut c);
    // Whatever the exact site (v4 completeness or v2 cited-digest), it must be
    // INVALID_EVIDENCE — the window is not quietly shrunk to skip run-1.
    let comp = compute_tier(&c, Lane::Pi, COMMIT, &TierParams::gated());
    assert!(
        matches!(comp.verdict, TierVerdict::InvalidEvidence { .. }),
        "a completed entry with its artifact omitted must be INVALID, not a smaller window: {:?}",
        comp.verdict
    );
}

// ---- CARDINALITY gaps + honest-twins --------------------------------------

#[test]
fn r6a_conflicting_terminal_transition_rejected_at_append() {
    // The append API is the positive-control: a second terminal for a run whose
    // terminal already stands is refused (corrections are new events, not overwrites).
    let battery = base_battery();
    let mut journal = AuthorityJournal::new();
    journal.activate_context(ContextActivationKind::Manifest(battery.manifest_digest()));
    journal
        .commission_run(RunId("run-x".into()), LaneScope::one(Lane::Pi), BoxId(BOX.into()), COMMIT,
            battery.manifest_digest(), battery.aggregation_version.clone(), RunKind::Evidence, COORD, &mut || "tok-x".into())
        .unwrap();
    // No run-start → cancellation-pre-execution is a VALID first terminal.
    journal.mark_terminal(&RunId("run-x".into()), TerminalState::CancelledPreExecution { reason: "first".into() }).unwrap();
    // A SECOND terminal transition for the same run is refused (one-way
    // disposition; corrections are new events, never overwrites — N-1).
    let second = journal.mark_terminal(
        &RunId("run-x".into()),
        TerminalState::Voided(super::journal::VoidRuling {
            authorized_by: "void-seat (cccc3333)".into(),
            no_artifact_evidence: "registrar confirms no artifact".into(),
        }),
    );
    let err = second.expect_err("a second terminal transition must be refused");
    assert!(err.contains("already terminal"), "expected an already-terminal refusal, got {err:?}");
}

#[test]
fn r6b_honest_cancelled_pre_execution_validates() {
    // TWIN: an honest cancelled-pre-execution entry (zero artifacts) does NOT
    // poison the corpus — it computes normally over the remaining runs.
    let mut c = ga_corpus();
    // Add a 4th run that is cancelled pre-execution with no artifact, above the window.
    add_cancelled_run(&mut c, "run-cancel");
    assert_computes(&c);
}

#[test]
fn r8_honest_cancellation_no_start_validates() {
    // TWIN (r8): an honest cancellation with no run-start validates cleanly.
    let mut c = ga_corpus();
    add_cancelled_run(&mut c, "run-cancel-nostart");
    assert_computes(&c);
}

#[test]
fn r6c_honest_voided_entry_validates() {
    // TWIN (r6): an honest VOIDED entry with zero artifacts does NOT poison the
    // corpus — it computes normally (the counterpart to the artifact-for-voided
    // negative that tier_tests covers).
    let mut c = ga_corpus();
    let battery = base_battery();
    c.journal
        .commission_run(RunId("run-void".into()), LaneScope::one(Lane::Pi), BoxId(BOX.into()), COMMIT,
            battery.manifest_digest(), battery.aggregation_version.clone(), RunKind::Evidence, COORD, &mut || "tok-void".into())
        .unwrap();
    c.journal
        .mark_terminal(&RunId("run-void".into()), TerminalState::Voided(super::journal::VoidRuling {
            authorized_by: "void-seat (cccc3333)".into(),
            no_artifact_evidence: "registrar confirms no artifact was produced".into(),
        }))
        .unwrap();
    reanchor(&mut c);
    assert_computes(&c);
}

/// Append a cancelled-pre-execution run (no start, no artifact) to a corpus,
/// then re-anchor. Honest: zero artifacts, terminal cancelled.
fn add_cancelled_run(corpus: &mut Corpus, run: &str) {
    let battery = base_battery();
    corpus
        .journal
        .commission_run(RunId(run.into()), LaneScope::one(Lane::Pi), BoxId(BOX.into()), COMMIT,
            battery.manifest_digest(), battery.aggregation_version.clone(), RunKind::Evidence, COORD, &mut || format!("tok-{run}"))
        .unwrap();
    corpus
        .journal
        .mark_terminal(&RunId(run.into()), TerminalState::CancelledPreExecution { reason: "box unavailable before any cell executed".into() })
        .unwrap();
    reanchor(corpus);
}

// ---- CITATION consume-twin (r12) ------------------------------------------

#[test]
fn r12_cited_artifact_is_consumed() {
    // The paired POSITIVE of the uncited-invisible case: with the citation
    // present, the artifact is consumed and the corpus computes (GA over 3 runs).
    // ga_corpus already cites every artifact via its completion events — so the
    // positive-control is that all three cited artifacts are consumed into GA.
    let comp = compute_tier(&ga_corpus(), Lane::Pi, COMMIT, &TierParams::gated());
    assert_eq!(comp.verdict, TierVerdict::Ga, "cited artifacts are consumed into the computed window");
    assert_eq!(comp.window.len(), 3, "all three cited runs are in the window (consumed, not invisible)");
}

// ---- ASSEMBLY-BINDING gap (r13b) ------------------------------------------
// NOTE: r13a ("header carries NO launch nonce") is UNCONSTRUCTIBLE — the
// CommissioningHeader.launch_nonce field is non-Option, so an artifact cannot
// serialize without a nonce (like R8-1's deliberately-absent pre-start negative).
// r13b (proof-of-run carries a nonce not bound to the header) IS constructible:

#[test]
fn r13b_green_proof_nonce_not_bound_to_header_rejected() {
    let mut c = ga_corpus();
    // Corrupt ONE green proof's launch_nonce in run-3's artifact so it no longer
    // binds to the (unchanged) header nonce — v8's proof-binding arm fires.
    // Serde repr: CellResolution is externally-tagged kebab (`observed`); Outcome
    // is internally-tagged (`#[serde(tag="outcome")]`) so ProofOfRun's fields are
    // flattened under `outcome` (no `/Pass` wrapper) → path `/observed/outcome`.
    let mut new_digest = None;
    for ia in c.index.artifacts.iter_mut() {
        if ia.artifact.header.tuple.run.0 == "run-3" {
            let mut val = serde_json::to_value(&ia.artifact).unwrap();
            if let Some(grid) = val["grid"].as_object_mut() {
                for (_k, res) in grid.iter_mut() {
                    if let Some(outcome) = res.pointer_mut("/observed/outcome") {
                        if outcome.get("launch_nonce").is_some() {
                            outcome["launch_nonce"] = serde_json::json!("nonce-unbound");
                            break;
                        }
                    }
                }
            }
            ia.artifact = serde_json::from_value(val).unwrap();
            ia.digest = ia.artifact.content_digest();
            new_digest = Some(ia.digest.0.clone());
        }
    }
    // Re-point run-3's completion citation at the recomputed digest so v2
    // (cited-digest) PASSES and validation reaches v8 (the intended check).
    let new_digest = new_digest.unwrap();
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == "run-3" {
                e["body"]["terminal"]["artifact_digest"] = serde_json::json!(new_digest);
            }
        }
    });
    reanchor(&mut c);
    assert_invalid(&c, "not bound to the artifact header");
}

#[test]
fn r13a_empty_launch_nonce_rejected() {
    // The truly-ABSENT nonce is unconstructible (CommissioningHeader.launch_nonce
    // is non-Option; serde requires it — mirroring R8-1's deliberately-absent
    // pre-start case). The meaningful "header carries no VALID nonce" sub-case is
    // an EMPTY nonce, which the journal's run-start integrity check rejects.
    let mut c = ga_corpus();
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "start" && e["body"]["run"] == "run-3" {
                e["body"]["launch_nonce"] = serde_json::json!("");
            }
        }
    });
    assert_invalid(&c, "empty launch nonce");
}

// ---- DESIGNATION gap (d3) -------------------------------------------------

#[test]
fn d3_duplicate_ordinals_rejected() {
    let mut c = ga_corpus();
    mutate_journal(&mut c, |v| {
        // Force two entries to share an ordinal.
        let arr = v["entries"].as_array_mut().unwrap();
        if arr.len() >= 2 {
            let first = arr[0]["ordinal"].clone();
            arr[1]["ordinal"] = first;
        }
    });
    assert_invalid(&c, "duplicate ordinal");
}

// ---- PUBLICATION-ANCHORING gap (p1 limb2) ---------------------------------

#[test]
fn p1_index_head_differs_from_snapshot_attested_digest_rejected() {
    // Re-publish the snapshot attesting a WRONG head digest (index untouched, so
    // no v2 duplicate/cardinality noise): the snapshot's attested digest no longer
    // equals the true index head → v7 limb 2, in isolation.
    let mut c = ga_corpus();
    mutate_journal(&mut c, |v| {
        v["entries"].as_array_mut().unwrap().retain(|e| e["body"]["event"] != "publication-snapshot");
    });
    c.journal.publish_snapshot(super::ids::ArtifactDigest("sha256:wrong-attested-head".into()), SNAP);
    assert_invalid(&c, "does not equal the snapshot's attested digest");
}

// ---- EXERCISE-KIND FORECLOSURE (coord-ruled, new) -------------------------

#[test]
fn exercise_kind_run_is_foreclosed_from_the_window() {
    // An Exercise-kind run, eligible by ordinal (completed, on the designated box,
    // within the window band), must be kept OUT of the window (R5-O3) — its
    // presence must not change the computed window vs the Evidence-only base.
    let base = compute_tier(&ga_corpus(), Lane::Pi, COMMIT, &TierParams::gated());
    let mut c = ga_corpus();
    add_completed_exercise_run(&mut c, "run-exercise");
    let with_ex = compute_tier(&c, Lane::Pi, COMMIT, &TierParams::gated());
    assert!(
        !with_ex.window.iter().any(|r| r.0 == "run-exercise"),
        "an Exercise-kind run must be foreclosed from the window (R5-O3), window={:?}",
        with_ex.window
    );
    assert_eq!(base.window, with_ex.window, "the Exercise run leaves the window unchanged (foreclosed, not counted)");
}

/// Append a COMPLETED Exercise-kind run with a full Pass grid + artifact.
fn add_completed_exercise_run(corpus: &mut Corpus, run: &str) {
    let battery = base_battery();
    let tuple = corpus
        .journal
        .commission_run(RunId(run.into()), LaneScope::one(Lane::Pi), BoxId(BOX.into()), COMMIT,
            battery.manifest_digest(), battery.aggregation_version.clone(), RunKind::Exercise, COORD, &mut || format!("tok-{run}"))
        .unwrap();
    let launch = corpus.journal.start_run_at(&tuple.run, RUNNER, "2026-07-14T12:00:00Z", &mut || format!("nonce-{run}")).unwrap();
    let header = CommissioningHeader::new(tuple, launch);
    let mut b = RunArtifactBuilder::new(header, "2026-07-14T12:00:00Z");
    let mut applicable = Vec::new();
    for cell in &battery.cells {
        let cid = cell.id.clone();
        b.observe(Lane::Pi, cid.clone(), RunMode::Automated, b.runner().pass(vec!["cmd".into()], "observed"));
        applicable.push((Lane::Pi, cid));
    }
    let artifact = b.build(&applicable).unwrap();
    let digest = artifact.content_digest();
    corpus.journal.mark_terminal(&RunId(run.into()), TerminalState::Completed { artifact_digest: digest.clone() }).unwrap();
    corpus.index.artifacts.push(IndexedArtifact { digest, artifact });
    reanchor(corpus);
}

// ---- DESIGNATION flip (d4 augmentation, coord-required) --------------------

/// A designation-flip corpus: `lima` designated + `brano` pending + `n` brano
/// Evidence-Completed runs. At n==W the promotion derives at the Wth brano run's
/// terminal-completion ordinal. Returns (corpus, brano-run-ids in order).
fn flip_corpus(n_brano: usize) -> (Corpus, Vec<String>) {
    let battery = base_battery();
    let mut journal = AuthorityJournal::new();
    journal.activate_context(ContextActivationKind::Manifest(battery.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(battery.aggregation_version.clone()));
    journal.designate(DesignationEvent {
        kind: DesignationKind::Designate, lane: Lane::Pi, box_id: BoxId(BOX.into()),
        basis_kind: Some(DesignationBasisKind::GreaterRunnability),
        policy_basis: "greater runnability".into(), rationale: "base designation".into(),
        authorized_by: "designation-seat (dddd4444)".into(), timestamp: "2026-07-13T00:00:00Z".into(),
    });
    journal.designate(DesignationEvent {
        kind: DesignationKind::PendingReplacement, lane: Lane::Pi, box_id: BoxId("brano".into()),
        basis_kind: Some(DesignationBasisKind::GreaterRunnability),
        policy_basis: "greater runnability".into(), rationale: "pending replacement".into(),
        authorized_by: "designation-seat (dddd4444)".into(), timestamp: "2026-07-13T06:00:00Z".into(),
    });
    let mut index = Vec::new();
    let mut brano_runs = Vec::new();
    for i in 0..n_brano {
        let run = format!("brano-run-{i}");
        let tuple = journal
            .commission_run(RunId(run.clone()), LaneScope::one(Lane::Pi), BoxId("brano".into()), COMMIT,
                battery.manifest_digest(), battery.aggregation_version.clone(), RunKind::Evidence, COORD, &mut || format!("tok-{run}"))
            .unwrap();
        let launch = journal.start_run_at(&tuple.run, RUNNER, "2026-07-14T00:00:00Z", &mut || format!("nonce-{run}")).unwrap();
        let header = CommissioningHeader::new(tuple, launch);
        let mut b = RunArtifactBuilder::new(header, "2026-07-14T00:00:00Z");
        let mut applicable = Vec::new();
        for cell in &battery.cells {
            let cid = cell.id.clone();
            b.observe(Lane::Pi, cid.clone(), RunMode::Automated, b.runner().pass(vec!["cmd".into()], "observed"));
            applicable.push((Lane::Pi, cid));
        }
        let artifact = b.build(&applicable).unwrap();
        let digest = artifact.content_digest();
        journal.mark_terminal(&RunId(run.clone()), TerminalState::Completed { artifact_digest: digest.clone() }).unwrap();
        index.push(IndexedArtifact { digest, artifact });
        brano_runs.push(run);
    }
    let head = ArtifactIndex { artifacts: index.clone() }.head_digest();
    journal.publish_snapshot(head, SNAP);
    let mut commits = BTreeMap::new();
    commits.insert(COMMIT.to_string(), CommitContext { battery });
    (Corpus { journal, index: ArtifactIndex { artifacts: index }, attributions: Vec::new(), commits, roles: RoleRegistry::default() }, brano_runs)
}

#[test]
fn d4_promotion_derives_at_the_wth_run_terminal_ordinal_exactly() {
    // AUGMENTATION (coord-required): the existing tier_tests flip test asserts
    // promotion_ordinal.is_some(); the spec demands the promotion derive AT the
    // Wth eligible run's terminal-completion ordinal EXACTLY. Pin it, + 2-reader.
    let (c, brano) = flip_corpus(3);
    let comp = compute_tier(&c, Lane::Pi, COMMIT, &TierParams::gated());
    assert_eq!(comp.active_box.as_deref(), Some("brano"), "at W the promotion flips the active box to brano");
    let wth = &brano[2];
    let jv = serde_json::to_value(&c.journal).unwrap();
    let wth_terminal_ord = jv["entries"].as_array().unwrap().iter()
        .find(|e| e["body"]["event"] == "terminal" && e["body"]["run"].as_str() == Some(wth.as_str()))
        .and_then(|e| e["ordinal"].as_u64());
    assert!(wth_terminal_ord.is_some(), "the Wth brano run has a terminal-completion event");
    assert_eq!(
        comp.promotion_ordinal, wth_terminal_ord,
        "the promotion must derive AT the Wth run's terminal-completion ordinal, exactly (not merely is_some)"
    );
    // Two independent readers derive the identical computation (byte-identical replay).
    let comp2 = compute_tier(&c, Lane::Pi, COMMIT, &TierParams::gated());
    assert_eq!(comp, comp2, "two independent readers derive the identical promotion");
}

// ---- PUBLICATION historical byte-identity (p2 gap) ------------------------

/// The registrar attestation for a corpus's latest snapshot (mirrors tier_tests).
fn registrar_of(corpus: &Corpus) -> super::tier_doc::RegistrarAttestation {
    let (ord, snap) = corpus.journal.latest_snapshot().expect("anchored corpus");
    super::tier_doc::RegistrarAttestation {
        latest_ordinal: ord.get(),
        attested_head_digest: snap.artifact_index_head_digest.clone(),
        observed_at: snap.observed_at.clone(),
    }
}

#[test]
fn p2_historical_rendering_reproduces_byte_identically() {
    use super::tier_doc::{publish_tier_document, PublicationHistory, PublicationMode};
    // Publish a current document, then render the SAME (older) anchor as HISTORICAL
    // TWICE — the gap tier_tests leaves open is byte-identical reproduction of the
    // historical rendering (legality + banner are covered there; byte-match is not).
    let current = ga_corpus();
    let mut history = PublicationHistory::default();
    publish_tier_document(&current, COMMIT, &TierParams::gated(), PublicationMode::Current, &registrar_of(&current), &mut history).unwrap();
    let doc1 = publish_tier_document(&current, COMMIT, &TierParams::gated(), PublicationMode::Historical, &registrar_of(&current), &mut history).unwrap();
    let doc2 = publish_tier_document(&current, COMMIT, &TierParams::gated(), PublicationMode::Historical, &registrar_of(&current), &mut history).unwrap();
    assert!(doc1.contains("HISTORICAL RENDERING"), "the historical rendering carries its banner");
    assert_eq!(doc1, doc2, "the historical rendering reproduces BYTE-IDENTICALLY across two renders (R6-5)");
}

// ---- DESIGNATION race (d1) + stale-ref (d2) -------------------------------

/// Append a completed Evidence run on `box_name` to a journal + index.
fn push_completed_run(
    journal: &mut AuthorityJournal,
    index: &mut Vec<IndexedArtifact>,
    battery: &Battery,
    run: &str,
    box_name: &str,
    tok_n: u64,
) {
    let tuple = journal
        .commission_run(RunId(run.into()), LaneScope::one(Lane::Pi), BoxId(box_name.into()), COMMIT,
            battery.manifest_digest(), battery.aggregation_version.clone(), RunKind::Evidence, COORD, &mut || format!("tok-{tok_n}"))
        .unwrap();
    let launch = journal.start_run_at(&tuple.run, RUNNER, "2026-07-14T00:00:00Z", &mut || format!("nonce-{tok_n}")).unwrap();
    let header = CommissioningHeader::new(tuple, launch);
    let mut b = RunArtifactBuilder::new(header, "2026-07-14T00:00:00Z");
    let mut applicable = Vec::new();
    for cell in &battery.cells {
        let cid = cell.id.clone();
        b.observe(Lane::Pi, cid.clone(), RunMode::Automated, b.runner().pass(vec!["cmd".into()], "observed"));
        applicable.push((Lane::Pi, cid));
    }
    let artifact = b.build(&applicable).unwrap();
    let digest = artifact.content_digest();
    journal.mark_terminal(&RunId(run.into()), TerminalState::Completed { artifact_digest: digest.clone() }).unwrap();
    index.push(IndexedArtifact { digest, artifact });
}

fn designate(journal: &mut AuthorityJournal, kind: DesignationKind, box_name: &str, ts: &str) {
    journal.designate(DesignationEvent {
        kind, lane: Lane::Pi, box_id: BoxId(box_name.into()),
        basis_kind: Some(DesignationBasisKind::GreaterRunnability),
        policy_basis: "greater runnability".into(), rationale: "designation".into(),
        authorized_by: "designation-seat (dddd4444)".into(), timestamp: ts.into(),
    });
}

fn wrap_corpus(journal: AuthorityJournal, index: Vec<IndexedArtifact>, battery: Battery) -> Corpus {
    let mut commits = BTreeMap::new();
    commits.insert(COMMIT.to_string(), CommitContext { battery });
    Corpus { journal, index: ArtifactIndex { artifacts: index }, attributions: Vec::new(), commits, roles: RoleRegistry::default() }
}

#[test]
fn d1_runs_land_on_the_correct_side_of_the_pending_boundary() {
    // A lima (active-box) run BEFORE the pending event, then 2 brano (pending-box)
    // runs AFTER: each lands on the correct side by journal ORDINAL. Below W the
    // active box stays lima and the accumulation counts EXACTLY the post-pending
    // brano runs — the pre-pending lima run does not inflate the pending count.
    let battery = base_battery();
    let mut journal = AuthorityJournal::new();
    journal.activate_context(ContextActivationKind::Manifest(battery.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(battery.aggregation_version.clone()));
    designate(&mut journal, DesignationKind::Designate, BOX, "2026-07-13T00:00:00Z");
    let mut index = Vec::new();
    push_completed_run(&mut journal, &mut index, &battery, "lima-pre", BOX, 0); // before pending
    designate(&mut journal, DesignationKind::PendingReplacement, "brano", "2026-07-13T06:00:00Z");
    push_completed_run(&mut journal, &mut index, &battery, "brano-0", "brano", 1); // after pending
    push_completed_run(&mut journal, &mut index, &battery, "brano-1", "brano", 2);
    let head = ArtifactIndex { artifacts: index.clone() }.head_digest();
    journal.publish_snapshot(head, SNAP);
    let c = wrap_corpus(journal, index, battery);
    let comp = compute_tier(&c, Lane::Pi, COMMIT, &TierParams::gated());
    assert_eq!(comp.active_box.as_deref(), Some("lima"), "below W the active box stays lima (runs landed on the correct side)");
    assert_eq!(comp.pending_box.as_deref(), Some("brano"), "brano is the pending box");
    assert_eq!(comp.accumulation, 2, "exactly the 2 post-pending brano runs accumulate; the pre-pending lima run is on the old side");
}

#[test]
fn d2_stale_designation_reference_rejected() {
    // A second PendingReplacement while a pending is already outstanding — the
    // designation state references a stale/superseded pending (v6). (The
    // nonexistent-reference half — a pending with no active designation — is
    // covered by tier_tests::a_pending_with_no_active_designation_is_invalid.)
    // NOTE: substring read at build from replay_designations; refine if the
    // second-pending path emits a different v6 message.
    let battery = base_battery();
    let mut journal = AuthorityJournal::new();
    journal.activate_context(ContextActivationKind::Manifest(battery.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(battery.aggregation_version.clone()));
    designate(&mut journal, DesignationKind::Designate, BOX, "2026-07-13T00:00:00Z");
    designate(&mut journal, DesignationKind::PendingReplacement, "brano", "2026-07-13T06:00:00Z");
    designate(&mut journal, DesignationKind::PendingReplacement, "carbo", "2026-07-13T07:00:00Z"); // 2nd pending
    let mut index = Vec::new();
    push_completed_run(&mut journal, &mut index, &battery, "run-1", BOX, 0);
    let head = ArtifactIndex { artifacts: index.clone() }.head_digest();
    journal.publish_snapshot(head, SNAP);
    let c = wrap_corpus(journal, index, battery);
    // Positive-control: NAME the intended check (per the (d) why-holder ruling
    // 01KXM9ZT1K4MHMGXD5A6HD9EF1) — a second outstanding PendingReplacement is
    // rejected by the designation-replay v6, not merely "some InvalidEvidence".
    assert_invalid(&c, "second PendingReplacement");
}

// ---- CONTEXT epoch twin (d6) ----------------------------------------------

#[test]
fn d6_promotion_under_an_earlier_manifest_replays_legal_under_its_epoch() {
    // TWIN: a designation earned under an EARLIER manifest (A) is NOT retroactively
    // invalidated when a LATER manifest (B) is activated — it replays legal under
    // its own epoch. The corpus computes (never INVALID_EVIDENCE from the epoch
    // change alone) and the designation history carries the A-epoch designation.
    let battery_a = base_battery();
    let mut battery_b = base_battery();
    battery_b.aggregation_version = AggregationVersion("agg-v2".into());
    let mut journal = AuthorityJournal::new();
    // Epoch A: activate A, designate under A, run 3 brano runs (manifest A).
    journal.activate_context(ContextActivationKind::Manifest(battery_a.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(battery_a.aggregation_version.clone()));
    designate(&mut journal, DesignationKind::Designate, BOX, "2026-07-13T00:00:00Z");
    designate(&mut journal, DesignationKind::PendingReplacement, "brano", "2026-07-13T06:00:00Z");
    let mut index = Vec::new();
    for i in 0..3 {
        push_completed_run(&mut journal, &mut index, &battery_a, &format!("brano-{i}"), "brano", i as u64);
    }
    // Epoch B: activate the later manifest B (release context becomes B).
    journal.activate_context(ContextActivationKind::Manifest(battery_b.manifest_digest()));
    journal.activate_context(ContextActivationKind::AggregationVersion(battery_b.aggregation_version.clone()));
    let head = ArtifactIndex { artifacts: index.clone() }.head_digest();
    journal.publish_snapshot(head, SNAP);
    let c = wrap_corpus(journal, index, battery_b); // release-derived context = B
    let comp = compute_tier(&c, Lane::Pi, COMMIT, &TierParams::gated());
    assert!(
        !matches!(comp.verdict, TierVerdict::InvalidEvidence { .. }),
        "the A-epoch designation must NOT be retroactively invalidated by activating manifest B, got {:?}",
        comp.verdict
    );
    assert!(
        !comp.designation_history.is_empty(),
        "the earlier-epoch designation history replays (not erased by the newer manifest)"
    );
}
