//! C-3 QS-4 — the **fresh-reader reproduction** fixtures and the preregistered
//! **negative corpora battery**. Each fixture assembles a full corpus (the
//! authenticated artifact index + the authority journal, all six event kinds)
//! and asserts the tier function reproduces the identical result — a **byte-match**
//! proves determinism; the semantics assertions prove it is the AUTHORIZED
//! computation.
//!
//! The positive corpus deliberately includes a timestamp tie, an unregistered
//! (uncited, corpus-invisible) run, a multi-lane run (the lane projection
//! reproduced), and an in-flight non-terminal entry above the candidate cutoff
//! (legal and inert while fresh). The negative battery drives every preregistered
//! malformation to its identical `INVALID_EVIDENCE` (or named/suppression)
//! result.

use std::collections::BTreeMap;

use super::attribution::{AttributionRecord, Disposition, ReasonCode};
use super::evidence::{
    BoxCoverageRecord, CommissioningHeader, Outcome, RunArtifact, RunArtifactBuilder, RunMode,
};
use super::ids::{
    AggregationVersion, AttributionSeq, BoxId, CellId, Lane, LaneScope, ObservationId, RecordId,
    RunId, RunKind,
};
use super::journal::{
    AbortRuling, AuthorityJournal, ContextActivationKind, DesignationBasisKind, DesignationEvent,
    DesignationKind, TerminalState, VoidRuling,
};

/// A descriptive companion string for a typed basis kind (T4/audit readability
/// only — never machine-checked).
fn describe_basis(kind: DesignationBasisKind) -> String {
    match kind {
        DesignationBasisKind::Decommissioned => "the box was decommissioned".into(),
        DesignationBasisKind::DurablyUnreachable => "the box became durably unreachable".into(),
        DesignationBasisKind::RunnabilityReduced => {
            "the lane's applicable-cell runnability was materially reduced".into()
        }
        DesignationBasisKind::GreaterRunnability => {
            "strictly greater applicable-cell runnability at representative shape".into()
        }
    }
}
use super::registry::{Applicability, Battery, Cell, Dimension};
use super::tier::{
    compute_tier, ArtifactIndex, Citation, CommitContext, Corpus, IndexedArtifact, RateValue,
    RoleRegistry, Tier, TierComputation, TierParams, TierVerdict,
};

// ===========================================================================
// The tiny battery — four cells (two gating, two non-gating) applicable to Pi
// and Codex, so full grids stay small and the lane projection is exercised.
// ===========================================================================

const C_D1: &str = "d1.g"; // D1 gating
const C_D6: &str = "d6.g"; // D6 gating
const C_D3: &str = "d3.n"; // D3 non-gating
const C_D5: &str = "d5.n"; // D5 non-gating

fn tiny_battery() -> Battery {
    let cells = vec![
        Cell {
            id: CellId(C_D1.into()),
            dimension: Dimension::D1Lifecycle,
            title: "gating d1".into(),
        },
        Cell {
            id: CellId(C_D6.into()),
            dimension: Dimension::D6FailureHonesty,
            title: "gating d6".into(),
        },
        Cell {
            id: CellId(C_D3.into()),
            dimension: Dimension::D3BusyQueue,
            title: "non-gating d3".into(),
        },
        Cell {
            id: CellId(C_D5.into()),
            dimension: Dimension::D5Transcript,
            title: "non-gating d5".into(),
        },
    ];
    let mut applicability = BTreeMap::new();
    for lane in [Lane::Pi, Lane::Codex] {
        for c in [C_D1, C_D6, C_D3, C_D5] {
            applicability.insert(
                (lane.id().to_string(), c.to_string()),
                Applicability::Required,
            );
        }
    }
    Battery {
        cells,
        applicability,
        aggregation_version: AggregationVersion("agg-v1".into()),
    }
}

const COMMIT: &str = "commit-r9";
const BOX: &str = "lima";
const SNAP_TIME: &str = "2026-07-15T12:00:00Z";

// ===========================================================================
// The step-based corpus builder — drives the REAL journal API so tokens,
// nonces, ordinals, and terminal preconditions are all internally consistent.
// Negative corpora mutate the assembled corpus.
// ===========================================================================

/// What a run resolves each of its planned cells to (default: pass). Some
/// variants are scaffolding for corpora the negative battery can grow into.
#[allow(dead_code)]
#[derive(Clone)]
enum Cellcome {
    Pass,
    Fail(String),
    Blocked,
    Na,
    Coverage,
}

/// A run to commission.
struct RunSpec {
    run: &'static str,
    lanes: Vec<Lane>,
    kind: RunKind,
    box_id: String,
    commit: String,
    started_at: Option<String>,
    terminal: Term,
    timestamp: String,
    /// per-(lane, cell) outcome override; unspecified cells pass.
    overrides: Vec<(Lane, &'static str, Cellcome)>,
    coordinator: String,
    runner: String,
}

impl RunSpec {
    fn evidence(run: &'static str) -> Self {
        RunSpec {
            run,
            lanes: vec![Lane::Pi],
            kind: RunKind::Evidence,
            box_id: BOX.into(),
            commit: COMMIT.into(),
            started_at: Some("2026-07-14T00:00:00Z".into()),
            terminal: Term::Completed,
            timestamp: "2026-07-14T00:00:00Z".into(),
            overrides: Vec::new(),
            coordinator: "coord-1 (aaaa1111)".into(),
            runner: "runner-1 (bbbb2222)".into(),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, PartialEq)]
enum Term {
    Completed,
    Cancelled,
    Voided,
    Aborted,
    Open,
}

enum Step {
    ActivateContext, // activates the tiny battery's manifest + aggregation version
    Designate {
        lane: Lane,
        box_id: &'static str,
        seat: &'static str,
        kind: DesignationBasisKind,
    },
    Pending {
        lane: Lane,
        box_id: &'static str,
        seat: &'static str,
        kind: DesignationBasisKind,
    },
    Withdraw {
        lane: Lane,
        box_id: &'static str,
        seat: &'static str,
        kind: DesignationBasisKind,
    },
    Run(RunSpec),
    Snapshot(&'static str),
}

struct Builder {
    journal: AuthorityJournal,
    index: Vec<IndexedArtifact>,
    battery: Battery,
    tok: u64,
    nonce: u64,
}

impl Builder {
    fn new() -> Self {
        Builder::with_battery(tiny_battery())
    }

    fn with_battery(battery: Battery) -> Self {
        Builder {
            journal: AuthorityJournal::new(),
            index: Vec::new(),
            battery,
            tok: 0,
            nonce: 0,
        }
    }

    fn run(mut self, steps: Vec<Step>) -> Corpus {
        for step in steps {
            self.step(step);
        }
        let mut commits = BTreeMap::new();
        commits.insert(
            COMMIT.to_string(),
            CommitContext {
                battery: self.battery.clone(),
            },
        );
        Corpus {
            journal: self.journal,
            index: ArtifactIndex {
                artifacts: self.index,
            },
            attributions: Vec::new(),
            commits,
            roles: RoleRegistry::default(),
        }
    }

    fn step(&mut self, step: Step) {
        match step {
            Step::ActivateContext => {
                self.journal
                    .activate_context(ContextActivationKind::Manifest(
                        self.battery.manifest_digest(),
                    ));
                self.journal
                    .activate_context(ContextActivationKind::AggregationVersion(
                        self.battery.aggregation_version.clone(),
                    ));
            }
            Step::Designate {
                lane,
                box_id,
                seat,
                kind,
            } => {
                self.journal.designate(DesignationEvent {
                    kind: DesignationKind::Designate,
                    lane,
                    box_id: BoxId(box_id.into()),
                    basis_kind: Some(kind),
                    policy_basis: describe_basis(kind),
                    rationale: "maximal applicable-cell runnability at representative shape".into(),
                    authorized_by: seat.into(),
                    timestamp: "2026-07-13T00:00:00Z".into(),
                });
            }
            Step::Pending {
                lane,
                box_id,
                seat,
                kind,
            } => {
                self.journal.designate(DesignationEvent {
                    kind: DesignationKind::PendingReplacement,
                    lane,
                    box_id: BoxId(box_id.into()),
                    basis_kind: Some(kind),
                    policy_basis: describe_basis(kind),
                    rationale: "a new box with strictly greater applicable-cell runnability".into(),
                    authorized_by: seat.into(),
                    timestamp: "2026-07-13T06:00:00Z".into(),
                });
            }
            Step::Withdraw {
                lane,
                box_id,
                seat,
                kind,
            } => {
                self.journal.designate(DesignationEvent {
                    kind: DesignationKind::Withdrawal,
                    lane,
                    box_id: BoxId(box_id.into()),
                    basis_kind: Some(kind),
                    policy_basis: describe_basis(kind),
                    rationale: "retire the pending slot".into(),
                    authorized_by: seat.into(),
                    timestamp: "2026-07-13T09:00:00Z".into(),
                });
            }
            Step::Run(spec) => self.commission_run(spec),
            Step::Snapshot(time) => {
                let head = ArtifactIndex {
                    artifacts: self.index.clone(),
                }
                .head_digest();
                self.journal.publish_snapshot(head, time);
            }
        }
    }

    fn commission_run(&mut self, spec: RunSpec) {
        let scope = if spec.lanes.len() == 1 {
            LaneScope::one(spec.lanes[0])
        } else {
            LaneScope::complete(spec.lanes.clone()).unwrap()
        };
        self.tok += 1;
        let mut mint_tok = {
            let t = self.tok;
            move || format!("tok-{t}")
        };
        let tuple = self
            .journal
            .commission_run(
                RunId(spec.run.into()),
                scope,
                BoxId(spec.box_id.clone()),
                spec.commit.clone(),
                self.battery.manifest_digest(),
                self.battery.aggregation_version.clone(),
                spec.kind,
                spec.coordinator.clone(),
                &mut mint_tok,
            )
            .unwrap();

        let nonce = if let Some(started_at) = &spec.started_at {
            self.nonce += 1;
            let n = self.nonce;
            Some(
                self.journal
                    .start_run_at(
                        &tuple.run,
                        spec.runner.clone(),
                        started_at.clone(),
                        &mut || format!("nonce-{n}"),
                    )
                    .unwrap(),
            )
        } else {
            None
        };

        match spec.terminal {
            Term::Completed => {
                let nonce = nonce.expect("a completed run must have started");
                let header = CommissioningHeader::new(tuple.clone(), nonce);
                let artifact = self.build_artifact(header, &spec);
                let digest = artifact.content_digest();
                self.journal
                    .mark_terminal(
                        &tuple.run,
                        TerminalState::Completed {
                            artifact_digest: digest.clone(),
                        },
                    )
                    .unwrap();
                self.index.push(IndexedArtifact { digest, artifact });
            }
            Term::Cancelled => {
                self.journal
                    .mark_terminal(
                        &tuple.run,
                        TerminalState::CancelledPreExecution {
                            reason: "box unavailable before any cell executed".into(),
                        },
                    )
                    .unwrap();
            }
            Term::Voided => {
                self.journal
                    .mark_terminal(
                        &tuple.run,
                        TerminalState::Voided(VoidRuling {
                            authorized_by: "void-seat (cccc3333)".into(),
                            no_artifact_evidence: "registrar confirms no artifact was produced"
                                .into(),
                        }),
                    )
                    .unwrap();
            }
            Term::Aborted => {
                self.journal
                    .mark_terminal(
                        &tuple.run,
                        TerminalState::Aborted(AbortRuling {
                            authorized_by: "void-seat (cccc3333)".into(),
                            no_artifact_evidence: "harness crash record ref-9 — infra failure"
                                .into(),
                        }),
                    )
                    .unwrap();
            }
            Term::Open => {}
        }
    }

    fn build_artifact(&self, header: CommissioningHeader, spec: &RunSpec) -> RunArtifact {
        let mut b = RunArtifactBuilder::new(header, spec.timestamp.clone());
        let mut applicable = Vec::new();
        for &lane in &spec.lanes {
            for cell in &self.battery.cells {
                if self.battery.applicability(lane, &cell.id).in_domain() {
                    let cid = cell.id.clone();
                    // Default resolution follows the manifest applicability so a
                    // mixed battery builds a full, permitted grid without per-cell
                    // overrides: Required→Pass, NaPermitted→Na, CoveragePermitted→
                    // Coverage. An explicit override wins.
                    let default = match self.battery.applicability(lane, &cid) {
                        Applicability::CoveragePermitted => Cellcome::Coverage,
                        Applicability::NaPermitted { .. } => Cellcome::Na,
                        _ => Cellcome::Pass,
                    };
                    let outcome_kind = spec
                        .overrides
                        .iter()
                        .find(|(l, c, _)| *l == lane && *c == cid.0)
                        .map(|(_, _, o)| o.clone())
                        .unwrap_or(default);
                    match outcome_kind {
                        Cellcome::Pass => b.observe(
                            lane,
                            cid.clone(),
                            RunMode::Automated,
                            b.runner().pass(vec!["cmd".into()], "observed state"),
                        ),
                        Cellcome::Fail(detail) => b.observe(
                            lane,
                            cid.clone(),
                            RunMode::Automated,
                            b.runner()
                                .fail(vec!["cmd".into()], "observed state", detail),
                        ),
                        Cellcome::Blocked => b.observe(
                            lane,
                            cid.clone(),
                            RunMode::Automated,
                            Outcome::blocked("gate closed", "open the gate"),
                        ),
                        Cellcome::Na => b.observe(
                            lane,
                            cid.clone(),
                            RunMode::Automated,
                            Outcome::not_applicable("not applicable here"),
                        ),
                        Cellcome::Coverage => b.cover(BoxCoverageRecord {
                            run: b.run_id().clone(),
                            lane,
                            cell: cid.clone(),
                            runnable_on: BoxId("other-box".into()),
                            why_not_here: "runnable only elsewhere".into(),
                            established_by: "coord-1 (aaaa1111)".into(),
                            reentry: "run on other-box".into(),
                        }),
                    }
                    applicable.push((lane, cid));
                }
            }
        }
        b.build(&applicable).unwrap()
    }
}

// ===========================================================================
// The canonical positive corpus: 3 eligible Pi runs (one multi-lane), a
// timestamp tie, an uncited unregistered artifact, and a fresh in-flight entry
// above the cutoff. All-pass → GA.
// ===========================================================================

fn positive_steps() -> Vec<Step> {
    let mut run1 = RunSpec::evidence("run-1");
    run1.timestamp = "2026-07-14T10:00:00Z".into(); // timestamp tie with run-2
    let mut run2 = RunSpec::evidence("run-2");
    run2.lanes = vec![Lane::Pi, Lane::Codex]; // the multi-lane run
    run2.timestamp = "2026-07-14T10:00:00Z".into(); // same descriptive timestamp
    let mut run3 = RunSpec::evidence("run-3");
    run3.timestamp = "2026-07-14T11:00:00Z".into();
    // A fresh in-flight entry ABOVE the candidate cutoff: started within Δ of the
    // snapshot, never terminal → legal and inert (not suppressed, not invalid).
    let mut inflight = RunSpec::evidence("run-inflight");
    inflight.started_at = Some("2026-07-15T11:00:00Z".into()); // 1h < Δ=24h before snapshot
    inflight.terminal = Term::Open;

    vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(run1),
        Step::Run(run2),
        Step::Run(run3),
        Step::Run(inflight),
        Step::Snapshot(SNAP_TIME),
    ]
}

fn positive_corpus() -> Corpus {
    let mut corpus = Builder::new().run(positive_steps());
    // The uncited "unregistered run": an artifact in the index whose run is NOT
    // in this journal (built via a throwaway journal), so it is corpus-invisible
    // — never a candidate and never a failure (R6-1).
    corpus.index.artifacts.push(unregistered_artifact());
    // Re-anchor: the snapshot must attest the FINAL index head (the extra artifact
    // changed it). Rebuild the snapshot at the current head.
    reanchor(&mut corpus, SNAP_TIME);
    corpus
}

/// An artifact whose run is registered in a SEPARATE journal (so it is
/// well-formed) but absent from the corpus journal and cited by nothing.
fn unregistered_artifact() -> IndexedArtifact {
    let mut j = AuthorityJournal::new();
    let battery = tiny_battery();
    let tuple = j
        .commission_run(
            RunId("run-unregistered".into()),
            LaneScope::one(Lane::Pi),
            BoxId(BOX.into()),
            COMMIT,
            battery.manifest_digest(),
            battery.aggregation_version.clone(),
            RunKind::Evidence,
            "coord-x (eeee5555)",
            &mut || "tok-unreg".to_string(),
        )
        .unwrap();
    let nonce = j
        .start_run_at(
            &tuple.run,
            "runner-x (ffff6666)",
            "2026-07-14T00:00:00Z",
            &mut || "nonce-unreg".to_string(),
        )
        .unwrap();
    let header = CommissioningHeader::new(tuple, nonce);
    let mut b = RunArtifactBuilder::new(header, "2026-07-14T00:00:00Z");
    let mut applicable = Vec::new();
    for cell in &battery.cells {
        if battery.applicability(Lane::Pi, &cell.id).in_domain() {
            b.observe(
                Lane::Pi,
                cell.id.clone(),
                RunMode::Automated,
                b.runner().pass(vec!["cmd".into()], "observed"),
            );
            applicable.push((Lane::Pi, cell.id.clone()));
        }
    }
    let artifact = b.build(&applicable).unwrap();
    let digest = artifact.content_digest();
    IndexedArtifact { digest, artifact }
}

/// Remove any publication-snapshot from the journal and append a fresh one at the
/// current index head (re-anchor after mutating the index or to advance "now").
fn reanchor(corpus: &mut Corpus, observed_at: &str) {
    let mut val = serde_json::to_value(&corpus.journal).unwrap();
    val["entries"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["body"]["event"] != "publication-snapshot");
    let mut j: AuthorityJournal = serde_json::from_value(val).unwrap();
    let head = corpus.index.head_digest();
    j.publish_snapshot(head, observed_at);
    corpus.journal = j;
}

// ===========================================================================
// Tests — the fresh-reader reproduction (positive) + byte-match determinism.
// ===========================================================================

const P: TierParams = TierParams::gated();

/// Serialize a computation to canonical JSON — the byte-match surface.
fn bytes(c: &TierComputation) -> String {
    serde_json::to_string(c).unwrap()
}

/// The fresh-reader reproduction: the same corpus + commit + scheme computes the
/// identical result, AND the computation survives a serialize→deserialize
/// round-trip of the corpus byte-for-byte (determinism / no ambient state).
fn assert_reproduces(corpus: &Corpus, lane: Lane) -> TierComputation {
    let a = compute_tier(corpus, lane, COMMIT, &P);
    // Round-trip the corpus through JSON (a fresh reader parses the artifact).
    let json = serde_json::to_string(corpus).unwrap();
    let reparsed: Corpus = serde_json::from_str(&json).unwrap();
    let b = compute_tier(&reparsed, lane, COMMIT, &P);
    assert_eq!(
        bytes(&a),
        bytes(&b),
        "byte-match reproduction failed for {lane:?}"
    );
    a
}

#[test]
fn positive_corpus_reproduces_ga_byte_for_byte() {
    let corpus = positive_corpus();
    let c = assert_reproduces(&corpus, Lane::Pi);
    assert_eq!(
        c.verdict,
        TierVerdict::Ga,
        "expected GA, got {:?}",
        c.verdict
    );
    assert_eq!(c.tier, Tier::Ga);
    // The window is exactly the 3 completed eligible runs, in (sequence, run-id)
    // order — never timestamp (run-1 and run-2 share a descriptive timestamp).
    assert_eq!(
        c.window.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        vec!["run-1", "run-2", "run-3"]
    );
    assert_eq!(c.evidence_run.as_ref().unwrap().0, "run-3");
    // Lane projection: Pi's pool is 3 runs × 4 Pi cells = 12, all pass (the
    // multi-lane run-2 contributes only its 4 Pi cells, not its Codex cells).
    assert_eq!(
        c.rate,
        Some(RateValue::Rate {
            passes: 12,
            pool: 12
        })
    );
    assert!(!c.stale);
    // Citation carries all six inputs.
    assert_citation_complete(&c.citation);
}

fn assert_citation_complete(cit: &Citation) {
    assert_eq!(cit.release_commit, COMMIT);
    assert!(cit.manifest_digest.is_some());
    assert!(cit.aggregation_version.is_some());
    assert!(cit.journal_content_digest.0.starts_with("sha256:"));
    assert!(cit.journal_high_water > 0);
    assert!(cit.index_head_digest.0.starts_with("sha256:"));
    assert!(cit.snapshot_ordinal.is_some());
    assert_eq!(cit.snapshot_attested_time.as_deref(), Some(SNAP_TIME));
}

#[test]
fn the_inflight_entry_above_the_cutoff_is_legal_and_inert() {
    // The positive corpus already carries a fresh in-flight run above the cutoff;
    // it must NOT invalidate and must NOT suppress — GA stands.
    let corpus = positive_corpus();
    let c = compute_tier(&corpus, Lane::Pi, COMMIT, &P);
    assert_eq!(c.verdict, TierVerdict::Ga);
}

#[test]
fn the_same_inflight_entry_reanchored_past_delta_suppresses() {
    // The SAME corpus re-anchored at a later snapshot, so the in-flight run's
    // start is now more than Δ old, reproduces the named suppression label — the
    // pair proves the deadline arithmetic (R6-1).
    let mut corpus = positive_corpus();
    reanchor(&mut corpus, "2026-07-17T00:00:00Z"); // >24h after the 2026-07-15T11 start
    let c = compute_tier(&corpus, Lane::Pi, COMMIT, &P);
    match &c.verdict {
        TierVerdict::Suppressed { entries, .. } => {
            assert!(
                entries.iter().any(|e| e.0 == "run-inflight"),
                "{:?}",
                entries
            );
        }
        other => panic!("expected suppression, got {other:?}"),
    }
    assert_eq!(c.tier, Tier::Experimental);
}

// ===========================================================================
// A minimal valid base corpus (3 Pi runs, no extras) → GA. Negatives mutate it.
// ===========================================================================

fn minimal_steps() -> Vec<Step> {
    vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]
}

fn minimal_corpus() -> Corpus {
    Builder::new().run(minimal_steps())
}

// ---- mutation helpers ------------------------------------------------------

/// Mutate the journal via its JSON representation (to forge invalid journals a
/// live API could never produce — the corpus-scope analogue of C-1's forge
/// tests). The structure must still deserialize; the invalidity is semantic.
fn mutate_journal(corpus: &mut Corpus, f: impl FnOnce(&mut serde_json::Value)) {
    let mut v = serde_json::to_value(&corpus.journal).unwrap();
    f(&mut v);
    corpus.journal = serde_json::from_value(v).unwrap();
}

/// Re-file a run's artifact after mutating it: recompute its content digest,
/// update the completion event's cited digest, and re-anchor.
fn refile_run(corpus: &mut Corpus, run: &str, f: impl FnOnce(&mut RunArtifact)) {
    let idx = corpus
        .index
        .artifacts
        .iter()
        .position(|a| a.artifact.header.tuple.run.0 == run)
        .expect("run in index");
    f(&mut corpus.index.artifacts[idx].artifact);
    let new_digest = corpus.index.artifacts[idx].artifact.content_digest();
    corpus.index.artifacts[idx].digest = new_digest.clone();
    mutate_journal(corpus, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == run {
                e["body"]["terminal"]["artifact_digest"] =
                    serde_json::Value::String(new_digest.0.clone());
            }
        }
    });
    reanchor(corpus, SNAP_TIME);
}

fn compute(corpus: &Corpus, lane: Lane) -> TierComputation {
    compute_tier(corpus, lane, COMMIT, &P)
}

#[track_caller]
fn assert_invalid(corpus: &Corpus, lane: Lane, substr: &str) {
    // Reproduce byte-for-byte, then assert the identical INVALID_EVIDENCE result.
    let c = assert_reproduces(corpus, lane);
    match &c.verdict {
        TierVerdict::InvalidEvidence { failed_checks } => {
            assert!(
                failed_checks.iter().any(|s| s.contains(substr)),
                "expected a failed check containing {substr:?}, got {failed_checks:#?}"
            );
        }
        other => panic!("expected INVALID_EVIDENCE containing {substr:?}, got {other:?}"),
    }
    assert_eq!(c.tier, Tier::Experimental);
}

#[track_caller]
fn assert_experimental(corpus: &Corpus, lane: Lane, substr: &str) {
    let c = assert_reproduces(corpus, lane);
    match &c.verdict {
        TierVerdict::Experimental { label } => assert!(
            label.contains(substr),
            "expected experimental label containing {substr:?}, got {label:?}"
        ),
        other => panic!("expected EXPERIMENTAL containing {substr:?}, got {other:?}"),
    }
}

// ---- sanity: the minimal base is GA -----------------------------------------

#[test]
fn minimal_base_is_ga() {
    let c = assert_reproduces(&minimal_corpus(), Lane::Pi);
    assert_eq!(c.verdict, TierVerdict::Ga);
}

// ===========================================================================
// NEGATIVE CORPORA BATTERY — each reproduces the identical INVALID_EVIDENCE (or
// named) result. Grouped by A10 clause.
// ===========================================================================

#[test]
fn neg_duplicate_copies() {
    let mut c = minimal_corpus();
    let dup = c.index.artifacts.last().unwrap().clone();
    c.index.artifacts.push(dup);
    assert_invalid(&c, Lane::Pi, "duplicate copies");
}

#[test]
fn neg_conflicting_same_id_artifacts() {
    let mut c = minimal_corpus();
    // A second, different artifact for run-3's id (a different timestamp → a
    // different content digest), filed alongside the canonical one.
    let idx = c
        .index
        .artifacts
        .iter()
        .position(|a| a.artifact.header.tuple.run.0 == "run-3")
        .unwrap();
    let mut other = c.index.artifacts[idx].clone();
    other.artifact.timestamp = "9999-01-01T00:00:00Z".into();
    other.digest = other.artifact.content_digest();
    c.index.artifacts.push(other);
    reanchor(&mut c, SNAP_TIME);
    assert_invalid(&c, Lane::Pi, "exactly one canonical artifact");
}

#[test]
fn neg_commission_token_bound_to_two_entries() {
    let mut c = minimal_corpus();
    // Force run-2's token to equal run-1's — a token under two run IDs (R5-O4).
    mutate_journal(&mut c, |v| {
        let mut tok1 = None;
        for e in v["entries"].as_array().unwrap() {
            if e["body"]["event"] == "run" && e["body"]["run"] == "run-1" {
                tok1 = Some(e["body"]["token"].clone());
            }
        }
        let tok1 = tok1.unwrap();
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "run" && e["body"]["run"] == "run-2" {
                e["body"]["token"] = tok1.clone();
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "duplicate commission token");
}

#[test]
fn neg_observation_for_a_lane_outside_the_commissioned_scope() {
    let mut c = minimal_corpus();
    // run-3 is commissioned for {Pi}; splice in a Codex observation.
    refile_run(&mut c, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Codex, &CellId(C_D1.into()));
        // borrow an existing Pi resolution's shape by cloning + relabeling.
        let pi_key = RunArtifact::cell_key(Lane::Pi, &CellId(C_D1.into()));
        let mut res = art.grid.get(&pi_key).unwrap().clone();
        if let super::evidence::CellResolution::Observed(o) = &mut res {
            o.lane = Lane::Codex;
            o.id = ObservationId::derive(&art.header.tuple.run, Lane::Codex, &CellId(C_D1.into()));
        }
        art.grid.insert(key, res);
    });
    assert_invalid(&c, Lane::Pi, "outside its commissioned scope");
}

#[test]
fn neg_extra_non_manifest_observation() {
    let mut c = minimal_corpus();
    refile_run(&mut c, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId("d9.not-in-manifest".into()));
        let pi_key = RunArtifact::cell_key(Lane::Pi, &CellId(C_D1.into()));
        let mut res = art.grid.get(&pi_key).unwrap().clone();
        if let super::evidence::CellResolution::Observed(o) = &mut res {
            o.cell = CellId("d9.not-in-manifest".into());
            o.id = ObservationId::derive(
                &art.header.tuple.run,
                Lane::Pi,
                &CellId("d9.not-in-manifest".into()),
            );
        }
        art.grid.insert(key, res);
    });
    assert_invalid(&c, Lane::Pi, "extra non-manifest observation");
}

#[test]
fn neg_missing_manifest_context_at_release_commit() {
    let c = minimal_corpus();
    // Compute at a release commit with no context artifact (F-3 derive fails).
    let comp = compute_tier(&c, Lane::Pi, "commit-does-not-exist", &P);
    match &comp.verdict {
        TierVerdict::InvalidEvidence { failed_checks } => assert!(
            failed_checks
                .iter()
                .any(|s| s.contains("no manifest/scheme-version context artifact")),
            "{failed_checks:#?}"
        ),
        other => panic!("expected INVALID, got {other:?}"),
    }
}

#[test]
fn neg_entry_claimed_context_disagrees_with_its_epoch() {
    let mut c = minimal_corpus();
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "run" && e["body"]["run"] == "run-2" {
                e["body"]["manifest_digest"] = serde_json::Value::String("sha256:bogus".into());
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "effective at its ordinal");
}

#[test]
fn neg_journal_latest_activation_disagrees_with_release_context() {
    let mut c = minimal_corpus();
    // Corrupt the activated manifest digest so the journal's latest effective
    // context no longer equals the release-derived one (R5-O7).
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "context-activation"
                && e["body"]["activation"]["activates"] == "manifest"
            {
                e["body"]["activation"]["context"] =
                    serde_json::Value::String("sha256:wrong".into());
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "disagrees with the release-derived");
}

#[test]
fn neg_run_preceding_any_context_activation() {
    // A run commissioned BEFORE any context activation → uncovered epoch (R5-O7).
    let steps = vec![
        Step::Run(RunSpec::evidence("run-early")),
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ];
    let c = Builder::new().run(steps);
    assert_invalid(&c, Lane::Pi, "precedes any manifest context-activation");
}

#[test]
fn neg_forged_promotion_event() {
    let mut c = minimal_corpus();
    // Inject a Designation event of kind "promotion" — no such event kind exists.
    mutate_journal(&mut c, |v| {
        let arr = v["entries"].as_array_mut().unwrap();
        let next = arr
            .iter()
            .map(|e| e["ordinal"].as_u64().unwrap())
            .max()
            .unwrap()
            + 1;
        arr.push(serde_json::json!({
            "ordinal": next,
            "body": {
                "event": "designation",
                "kind": "promotion",
                // The wire lane token, taken from `Lane::id` rather than
                // spelled by hand: this forgery has to DESERIALIZE (the point is
                // a semantically invalid journal, not an unparseable one), so a
                // stale literal here fails as a parse error and stops testing
                // what the test is named for.
                "lane": Lane::Pi.id(),
                "box_id": "lima",
                "policy_basis": "forged",
                "rationale": "forged promotion",
                "authorized_by": "someone (0000)",
                "timestamp": "2026-07-15T00:00:00Z"
            }
        }));
    });
    assert_invalid(&c, Lane::Pi, "forged promotion event");
}

#[test]
fn neg_absent_cited_digest() {
    let mut c = minimal_corpus();
    // Point run-3's completion at a digest not present in the index.
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == "run-3" {
                e["body"]["terminal"]["artifact_digest"] =
                    serde_json::Value::String("sha256:absent".into());
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "absent from the index");
}

#[test]
fn neg_completion_without_run_start() {
    let mut c = minimal_corpus();
    // Remove run-3's run-start event; its completion now has no start (R6-6).
    mutate_journal(&mut c, |v| {
        v["entries"]
            .as_array_mut()
            .unwrap()
            .retain(|e| !(e["body"]["event"] == "start" && e["body"]["run"] == "run-3"));
    });
    assert_invalid(&c, Lane::Pi, "prior run-start");
}

#[test]
fn neg_cancellation_following_a_run_start() {
    let mut c = minimal_corpus();
    // Change run-3's terminal to cancelled-pre-execution while its start stands.
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == "run-3" {
                e["body"]["terminal"] = serde_json::json!({
                    "state": "cancelled-pre-execution",
                    "reason": "forged cancel after start"
                });
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "cancellation after a run-start");
}

#[test]
fn neg_commissioning_identity_equals_runner_identity() {
    let mut c = minimal_corpus();
    mutate_journal(&mut c, |v| {
        // Set run-3's run-start runner identity equal to its commissioning identity.
        let coord = "coord-1 (aaaa1111)";
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "start" && e["body"]["run"] == "run-3" {
                e["body"]["runner_identity"] = serde_json::Value::String(coord.into());
            }
        }
    });
    assert_invalid(
        &c,
        Lane::Pi,
        "commissioning identity equals runner identity",
    );
}

#[test]
fn neg_duplicate_launch_nonce_under_two_entries() {
    let mut c = minimal_corpus();
    mutate_journal(&mut c, |v| {
        let mut n1 = None;
        for e in v["entries"].as_array().unwrap() {
            if e["body"]["event"] == "start" && e["body"]["run"] == "run-1" {
                n1 = Some(e["body"]["launch_nonce"].clone());
            }
        }
        let n1 = n1.unwrap();
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "start" && e["body"]["run"] == "run-2" {
                e["body"]["launch_nonce"] = n1.clone();
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "duplicate launch nonce");
}

#[test]
fn neg_header_nonce_mismatch() {
    let mut c = minimal_corpus();
    refile_run(&mut c, "run-3", |art| {
        art.header.launch_nonce = super::ids::LaunchNonce::new("nonce-bogus");
    });
    assert_invalid(&c, Lane::Pi, "nonce");
}

#[test]
fn neg_abort_ruling_shadows_a_cited_artifact() {
    let mut c = minimal_corpus();
    // Change run-3's terminal to Aborted while its artifact remains in the index.
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == "run-3" {
                e["body"]["terminal"] = serde_json::json!({
                    "state": "aborted",
                    "authorized_by": "void-seat (cccc3333)",
                    "no_artifact_evidence": "claims no artifact — but one is indexed"
                });
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "can never shadow evidence");
}

#[test]
fn neg_abort_ruling_by_non_independent_seat() {
    let mut c = minimal_corpus();
    // An aborted (started, no artifact) run whose abort seat IS the runner.
    // Build it explicitly: replace run-3 with an aborted run authorized by runner.
    let steps = vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ];
    c = Builder::new().run(steps);
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == "run-3" {
                e["body"]["terminal"] = serde_json::json!({
                    "state": "aborted",
                    "authorized_by": "runner-1 (bbbb2222)",
                    "no_artifact_evidence": "crash"
                });
            }
        }
        // Also drop run-3's index artifact so the abort doesn't shadow it — we are
        // testing the SEAT independence, not the shadow rule.
    });
    c.index
        .artifacts
        .retain(|a| a.artifact.header.tuple.run.0 != "run-3");
    reanchor(&mut c, SNAP_TIME);
    assert_invalid(&c, Lane::Pi, "independent void-and-abort seat");
}

#[test]
fn neg_corpus_not_anchored_at_a_snapshot() {
    let mut c = minimal_corpus();
    mutate_journal(&mut c, |v| {
        v["entries"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["body"]["event"] != "publication-snapshot");
    });
    assert_invalid(&c, Lane::Pi, "not anchored at any publication-snapshot");
}

#[test]
fn neg_non_terminal_registered_attempt_below_the_cutoff() {
    // A registered attempt (commissioned + started, never terminal) at a sequence
    // below the candidate cutoff → INVALID (stage 4, fail-closed).
    let mut open = RunSpec::evidence("run-open");
    open.terminal = Term::Open;
    let steps = vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(open),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ];
    let c = Builder::new().run(steps);
    assert_invalid(&c, Lane::Pi, "non-terminal");
}

// ---- named EXPERIMENTAL / suppression results (not INVALID) ------------------

#[test]
fn zero_candidate_corpus_is_named_experimental() {
    let steps = vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Snapshot(SNAP_TIME),
    ];
    let c = Builder::new().run(steps);
    assert_experimental(&c, Lane::Pi, "no eligible completed runs");
}

#[test]
fn short_window_is_named_experimental() {
    // Two completed eligible runs (< W=3) → window not accumulated.
    let steps = vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Snapshot(SNAP_TIME),
    ];
    let c = Builder::new().run(steps);
    assert_experimental(&c, Lane::Pi, "window not accumulated");
}

// ===========================================================================
// POSITIVE-SPACE TIER DISCRIMINATION — beta, gating-red, attribution.
// ===========================================================================

fn corpus_with_evrun_fail(cell: &'static str) -> Corpus {
    let mut run3 = RunSpec::evidence("run-3");
    run3.overrides = vec![(Lane::Pi, cell, Cellcome::Fail("injected".into()))];
    Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(run3),
        Step::Snapshot(SNAP_TIME),
    ])
}

#[test]
fn a_non_gating_fail_on_the_evidence_run_demotes_ga_to_beta() {
    let c = corpus_with_evrun_fail(C_D3); // d3 is non-gating
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(comp.verdict, TierVerdict::Beta, "got {:?}", comp.verdict);
    // 11/12 = 91.6% ≥ 80% (beta) but < 95% and not all-green (no GA).
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 11,
            pool: 12
        })
    );
}

#[test]
fn a_gating_fail_on_the_evidence_run_is_experimental() {
    let c = corpus_with_evrun_fail(C_D1); // d1 is gating
    assert_experimental(&c, Lane::Pi, "gating unmeasured or red");
}

#[test]
fn a_valid_harness_attribution_removes_a_fail_from_the_pool() {
    let mut c = corpus_with_evrun_fail(C_D3);
    // A valid harness ruling by a neutral seat removes the d3 fail from num+denom.
    let obs = ObservationId::derive(&RunId("run-3".into()), Lane::Pi, &CellId(C_D3.into()));
    c.attributions.push(AttributionRecord {
        id: RecordId("attr-1".into()),
        sequence: AttributionSeq::new(1),
        run: RunId("run-3".into()),
        observations: vec![obs],
        disposition: Disposition::Harness,
        reason: ReasonCode::HarnessFlakeTiming,
        primary_evidence: vec!["ci-log-ref-1".into()],
        issuer: "attributor (99998888)".into(),
        timestamp: "2026-07-15T00:00:00Z".into(),
        supersedes: vec![],
    });
    let comp = assert_reproduces(&c, Lane::Pi);
    // The fail is removed from num AND denom → pool 11/11.
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 11,
            pool: 11
        })
    );
    // Still not GA — on the evidence run a harness-removed fail is UNMEASURED, not
    // green (A5) → beta.
    assert_eq!(comp.verdict, TierVerdict::Beta);
}

#[test]
fn an_attribution_by_the_runs_executor_is_unauthorized_and_ignored() {
    let mut c = corpus_with_evrun_fail(C_D3);
    let obs = ObservationId::derive(&RunId("run-3".into()), Lane::Pi, &CellId(C_D3.into()));
    // Same ruling, but issued by the run's EXECUTOR (runner identity) → A8-invalid.
    c.attributions.push(AttributionRecord {
        id: RecordId("attr-bad".into()),
        sequence: AttributionSeq::new(1),
        run: RunId("run-3".into()),
        observations: vec![obs],
        disposition: Disposition::Harness,
        reason: ReasonCode::HarnessFlakeTiming,
        primary_evidence: vec!["ci-log-ref-1".into()],
        issuer: "runner-1 (bbbb2222)".into(),
        timestamp: "2026-07-15T00:00:00Z".into(),
        supersedes: vec![],
    });
    let comp = assert_reproduces(&c, Lane::Pi);
    // The unauthorized ruling never governs — the fail stays product-counted (A6).
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 11,
            pool: 12
        })
    );
}

// ===========================================================================
// DESIGNATION replay / promotion / withdrawal (A9).
// ===========================================================================

fn designation_steps(
    devbox_runs: usize,
    withdraw: Option<(&'static str, DesignationBasisKind)>,
) -> Vec<Step> {
    let mut steps = vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
    ];
    for i in 0..devbox_runs {
        let name: &'static str = Box::leak(format!("devbox-run-{i}").into_boxed_str());
        let mut spec = RunSpec::evidence(name);
        spec.box_id = "devbox".into();
        steps.push(Step::Run(spec));
    }
    if let Some((seat, kind)) = withdraw {
        steps.push(Step::Withdraw {
            lane: Lane::Pi,
            box_id: "devbox",
            seat,
            kind,
        });
    }
    steps.push(Step::Snapshot(SNAP_TIME));
    steps
}

#[test]
fn mid_transition_at_w_minus_1_keeps_the_old_active_box() {
    // Two eligible devbox runs (< W=3) → pending stands, active box unchanged.
    let c = Builder::new().run(designation_steps(2, None));
    let comp = compute(&c, Lane::Pi);
    assert_eq!(comp.active_box.as_deref(), Some("lima"));
    assert_eq!(comp.pending_box.as_deref(), Some("devbox"));
    assert_eq!(comp.promotion_ordinal, None);
    assert_eq!(comp.accumulation, 2);
}

#[test]
fn mid_transition_at_w_derives_the_promotion_flip() {
    // Three eligible devbox runs (= W) → promotion derives, devbox becomes active.
    let c = Builder::new().run(designation_steps(3, None));
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.active_box.as_deref(),
        Some("devbox"),
        "promotion should flip active box"
    );
    assert_eq!(comp.pending_box, None);
    assert!(comp.promotion_ordinal.is_some());
}

#[test]
fn a_quiet_window_withdrawal_is_valid() {
    // Pending devbox, no started-not-terminal devbox run at the withdrawal → valid.
    // Withdrawal basis is outcome-independent (box unreachable). After withdrawal
    // the pending is retired; Pi (active lima, zero lima runs) is a named
    // experimental, NOT invalid — proving the withdrawal was accepted.
    let c = Builder::new().run(designation_steps(
        0,
        Some((
            "designation-seat (dddd4444)",
            DesignationBasisKind::DurablyUnreachable,
        )),
    ));
    let comp = assert_reproduces(&c, Lane::Pi);
    assert!(
        matches!(comp.verdict, TierVerdict::Experimental { .. }),
        "expected a clean experimental, got {:?}",
        comp.verdict
    );
    assert_eq!(
        comp.pending_box, None,
        "the withdrawal retired the pending slot"
    );
}

#[test]
fn an_outcome_dependent_withdrawal_is_invalid() {
    // Under the typed enum an outcome-dependent withdrawal has NO valid kind — the
    // actor can only null it (the descriptive text is not machine-checked). INVALID.
    let mut c = Builder::new().run(designation_steps(
        0,
        Some((
            "designation-seat (dddd4444)",
            DesignationBasisKind::DurablyUnreachable,
        )),
    ));
    null_basis_kind(
        &mut c,
        "withdrawal",
        "the pending window had the worse pass rate",
    );
    assert_invalid(
        &c,
        Lane::Pi,
        "not an admissible outcome-independent withdrawal kind",
    );
}

#[test]
fn a_quiet_window_violating_withdrawal_is_invalid() {
    // A started-not-terminal devbox evidence run at the withdrawal ordinal (R6-2).
    let mut open = RunSpec::evidence("devbox-open");
    open.box_id = "devbox".into();
    open.terminal = Term::Open;
    let steps = vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(open),
        Step::Withdraw {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::DurablyUnreachable,
        },
        Step::Snapshot(SNAP_TIME),
    ];
    let c = Builder::new().run(steps);
    assert_invalid(&c, Lane::Pi, "quiet window");
}

#[test]
fn a_post_promotion_withdrawal_is_invalid() {
    // Three devbox runs promote; a withdrawal after the derived promotion ordinal.
    let c = Builder::new().run(designation_steps(
        3,
        Some((
            "designation-seat (dddd4444)",
            DesignationBasisKind::DurablyUnreachable,
        )),
    ));
    assert_invalid(&c, Lane::Pi, "at or after the derived promotion ordinal");
}

#[test]
fn a_pending_with_no_active_designation_is_invalid() {
    let steps = vec![
        Step::ActivateContext,
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Snapshot(SNAP_TIME),
    ];
    let c = Builder::new().run(steps);
    assert_invalid(&c, Lane::Pi, "no active designation");
}

// ===========================================================================
// (e) provider badge + (f) T4 document.
// ===========================================================================

#[test]
fn provider_badge_is_the_lanes_tier() {
    let c = minimal_corpus();
    assert_eq!(
        super::tier::provider_badge(&c, "pi", COMMIT, &P),
        Some(Tier::Ga)
    );
    // codex has no runs on its (absent) designated box → experimental.
    assert_eq!(
        super::tier::provider_badge(&c, "codex", COMMIT, &P),
        Some(Tier::Experimental)
    );
}

#[test]
fn t4_document_makes_the_interpretation_fidelity_visible() {
    let c = positive_corpus();
    let doc = super::tier_doc::render_tier_document(&c, COMMIT, &P);
    // (i) lanes not providers, (ii) window not single-run, (iii) demotes not blocks.
    assert!(doc.contains("Lanes, not providers"));
    assert!(doc.contains("window, not a single run") || doc.contains("A window, not a single run"));
    assert!(doc.contains("demotes; it does not block"));
    // Honest staleness + the six citation inputs + how to re-run.
    assert!(doc.contains("stale") || doc.contains("Honest"));
    assert!(doc.contains("Manifest digest"));
    assert!(doc.contains("Authority-journal version"));
    assert!(doc.contains("Anchoring publication-snapshot"));
    assert!(doc.contains("Re-running the battery"));
    // The current-as-of attested time and a GA lane assignment for pi.
    assert!(doc.contains(SNAP_TIME));
    assert!(doc.contains("`pi`"));
    // Deterministic: identical render on a re-parse of the corpus.
    let json = serde_json::to_string(&c).unwrap();
    let reparsed: Corpus = serde_json::from_str(&json).unwrap();
    assert_eq!(
        doc,
        super::tier_doc::render_tier_document(&reparsed, COMMIT, &P)
    );
}

// ---- additional cardinality / terminal-state fixtures ----------------------

#[test]
fn neg_voided_entry_with_an_artifact_shadows_evidence() {
    let mut c = minimal_corpus();
    // Change run-3's terminal to Voided while its artifact remains indexed — a
    // void can never shadow evidence (N-1).
    mutate_journal(&mut c, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "terminal" && e["body"]["run"] == "run-3" {
                e["body"]["terminal"] = serde_json::json!({
                    "state": "voided",
                    "authorized_by": "void-seat (cccc3333)",
                    "no_artifact_evidence": "claims no artifact — but one is indexed"
                });
            }
        }
    });
    assert_invalid(&c, Lane::Pi, "can never shadow evidence");
}

#[test]
fn neg_cancelled_entry_with_an_artifact_shadows_evidence() {
    // A cancelled-pre-execution run (no start) that nonetheless has an artifact in
    // the index for it — a non-producing terminal can never shadow evidence.
    let mut cancel = RunSpec::evidence("run-cancel");
    cancel.started_at = None; // cancellation requires the ABSENCE of a run-start
    cancel.terminal = Term::Cancelled;
    let mut c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Run(cancel),
        Step::Snapshot(SNAP_TIME),
    ]);
    // Push a standalone artifact whose run id is the cancelled entry's.
    c.index.artifacts.push(artifact_for_run("run-cancel"));
    reanchor(&mut c, SNAP_TIME);
    assert_invalid(&c, Lane::Pi, "can never shadow evidence");
}

/// A well-formed artifact for `run` (registered in a throwaway journal so the
/// header is valid) — for shadow fixtures that place an artifact against a
/// non-producing terminal.
fn artifact_for_run(run: &str) -> IndexedArtifact {
    let mut j = AuthorityJournal::new();
    let battery = tiny_battery();
    let tuple = j
        .commission_run(
            RunId(run.into()),
            LaneScope::one(Lane::Pi),
            BoxId(BOX.into()),
            COMMIT,
            battery.manifest_digest(),
            battery.aggregation_version.clone(),
            RunKind::Evidence,
            "coord-x (eeee5555)",
            &mut || format!("tok-{run}"),
        )
        .unwrap();
    let nonce = j
        .start_run_at(
            &tuple.run,
            "runner-x (ffff6666)",
            "2026-07-14T00:00:00Z",
            &mut || format!("nonce-{run}"),
        )
        .unwrap();
    let header = CommissioningHeader::new(tuple, nonce);
    let mut b = RunArtifactBuilder::new(header, "2026-07-14T00:00:00Z");
    let mut applicable = Vec::new();
    for cell in &battery.cells {
        if battery.applicability(Lane::Pi, &cell.id).in_domain() {
            b.observe(
                Lane::Pi,
                cell.id.clone(),
                RunMode::Automated,
                b.runner().pass(vec!["cmd".into()], "observed"),
            );
            applicable.push((Lane::Pi, cell.id.clone()));
        }
    }
    let artifact = b.build(&applicable).unwrap();
    let digest = artifact.content_digest();
    IndexedArtifact { digest, artifact }
}

#[test]
fn an_aborted_entry_with_a_valid_ruling_validates_cleanly() {
    // 3 completed runs + one aborted (started, no artifact, valid independent
    // ruling) below the cutoff → the aborted entry is terminal (v4 clean) and
    // excluded from the window → GA still stands.
    let mut abort = RunSpec::evidence("run-abort");
    abort.terminal = Term::Aborted;
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(abort),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.verdict,
        TierVerdict::Ga,
        "aborted entry must not block GA: {:?}",
        comp.verdict
    );
    // The aborted run is not in the window.
    assert!(!comp.window.iter().any(|r| r.0 == "run-abort"));
}

#[test]
fn an_exercise_run_never_enters_the_window() {
    // An exercise-kind run on the designated box is NEVER a window candidate
    // (A2(a)/R5-O3) — three evidence runs + one exercise → the exercise is
    // excluded; GA rests on the three evidence runs.
    let mut exercise = RunSpec::evidence("run-exercise");
    exercise.kind = RunKind::Exercise;
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(exercise),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(comp.verdict, TierVerdict::Ga);
    assert!(!comp.window.iter().any(|r| r.0 == "run-exercise"));
}

// ===========================================================================
// FIX-2 — F-3 ordering: a DISCRIMINATING fixture. The window/evrun MUST differ
// between sort-by-sequence (correct) and sort-by-timestamp (the F-3 regression).
// ===========================================================================

#[test]
fn window_selection_orders_by_sequence_not_timestamp() {
    // Four eligible runs. run-4 has the LATEST sequence but the EARLIEST
    // timestamp; run-1 has a non-gating fail and a middle timestamp.
    //   - by SEQUENCE (correct): window = last 3 = [run-2, run-3, run-4],
    //     run-1's fail EXCLUDED, evrun = run-4 → GA (12/12).
    //   - by TIMESTAMP (the F-3 bug): order = run-4(01:00), run-1(03:00),
    //     run-2(04:00), run-3(05:00) → window = [run-1, run-2, run-3], run-1's
    //     fail INCLUDED, evrun = run-3 → beta (11/12).
    // The tier itself differs, so this fixture FAILS if the sort key is switched
    // to timestamp — a real F-3 regression can no longer survive the battery.
    let mut run1 = RunSpec::evidence("run-1");
    run1.timestamp = "2026-07-14T03:00:00Z".into();
    run1.overrides = vec![(Lane::Pi, C_D3, Cellcome::Fail("non-gating".into()))];
    let mut run2 = RunSpec::evidence("run-2");
    run2.timestamp = "2026-07-14T04:00:00Z".into();
    let mut run3 = RunSpec::evidence("run-3");
    run3.timestamp = "2026-07-14T05:00:00Z".into();
    let mut run4 = RunSpec::evidence("run-4");
    run4.timestamp = "2026-07-14T01:00:00Z".into(); // latest sequence, EARLIEST time

    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(run1),
        Step::Run(run2),
        Step::Run(run3),
        Step::Run(run4),
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    // Sequence-ordered window + evrun (the F-3-correct result).
    assert_eq!(
        comp.window.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        vec!["run-2", "run-3", "run-4"],
        "window must be the sequence-ordered last-3, not the timestamp-ordered set"
    );
    assert_eq!(comp.evidence_run.as_ref().unwrap().0, "run-4");
    // GA: run-1's non-gating fail is OUTSIDE the sequence window → 12/12.
    assert_eq!(comp.verdict, TierVerdict::Ga, "got {:?}", comp.verdict);
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 12,
            pool: 12
        })
    );
    // NEGATIVE CONTROL (documented): under sort-by-timestamp the window would be
    // [run-1, run-2, run-3] / evrun run-3, run-1's fail INSIDE → beta 11/12, so
    // every assertion above flips — the fixture cannot pass under the F-3 bug.
}

// ===========================================================================
// FIX-3 — A9 coverage-record semantics. Batteries with a CoveragePermitted cell
// exercise the three downward effects.
// ===========================================================================

/// A battery with a CoveragePermitted NON-gating cell (d5.cov) for Pi — exercises
/// coverage-excluded-from-pool + coverage-fails-GA-all-green.
fn coverage_battery() -> Battery {
    use super::registry::{Applicability, Cell, Dimension};
    let cells = vec![
        Cell {
            id: CellId(C_D1.into()),
            dimension: Dimension::D1Lifecycle,
            title: "gating d1".into(),
        },
        Cell {
            id: CellId(C_D6.into()),
            dimension: Dimension::D6FailureHonesty,
            title: "gating d6".into(),
        },
        Cell {
            id: CellId(C_D3.into()),
            dimension: Dimension::D3BusyQueue,
            title: "non-gating d3".into(),
        },
        Cell {
            id: CellId("d5.cov".into()),
            dimension: Dimension::D5Transcript,
            title: "coverage non-gating".into(),
        },
    ];
    let mut applicability = BTreeMap::new();
    for c in [C_D1, C_D6, C_D3] {
        applicability.insert(
            (Lane::Pi.id().to_string(), c.to_string()),
            Applicability::Required,
        );
    }
    applicability.insert(
        (Lane::Pi.id().to_string(), "d5.cov".to_string()),
        Applicability::CoveragePermitted,
    );
    Battery {
        cells,
        applicability,
        aggregation_version: AggregationVersion("agg-v1".into()),
    }
}

/// A battery where a GATING cell (d6.cov, D6) is CoveragePermitted for Pi —
/// exercises gating-cell-by-coverage → unmeasured → experimental.
fn coverage_gating_battery() -> Battery {
    use super::registry::{Applicability, Cell, Dimension};
    let cells = vec![
        Cell {
            id: CellId(C_D1.into()),
            dimension: Dimension::D1Lifecycle,
            title: "gating d1".into(),
        },
        Cell {
            id: CellId("d6.cov".into()),
            dimension: Dimension::D6FailureHonesty,
            title: "coverage GATING".into(),
        },
        Cell {
            id: CellId(C_D3.into()),
            dimension: Dimension::D3BusyQueue,
            title: "non-gating d3".into(),
        },
    ];
    let mut applicability = BTreeMap::new();
    for c in [C_D1, C_D3] {
        applicability.insert(
            (Lane::Pi.id().to_string(), c.to_string()),
            Applicability::Required,
        );
    }
    applicability.insert(
        (Lane::Pi.id().to_string(), "d6.cov".to_string()),
        Applicability::CoveragePermitted,
    );
    Battery {
        cells,
        applicability,
        aggregation_version: AggregationVersion("agg-v1".into()),
    }
}

fn coverage_steps() -> Vec<Step> {
    vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]
}

#[test]
fn a_coverage_record_is_excluded_from_the_pool_and_blocks_ga() {
    // d5.cov auto-resolves to a coverage record on every run. Effects:
    //  (3) excluded from the pool → pool is d1.g/d6.g/d3.n only = 9/9, NOT 12/12;
    //  (2) an applicable coverage cell fails GA's all-green clause → beta, not GA.
    let c = Builder::with_battery(coverage_battery()).run(coverage_steps());
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate { passes: 9, pool: 9 }),
        "coverage cell must be OUT of the pool"
    );
    assert_eq!(
        comp.verdict,
        TierVerdict::Beta,
        "coverage cell must block GA all-green: {:?}",
        comp.verdict
    );
    // NEGATIVE CONTROL: if coverage were counted in the pool it'd be 9/12 or
    // 12/12; if it didn't block GA all-green the verdict would be GA. Both flip.
}

#[test]
fn a_gating_cell_resolved_by_coverage_is_experimental() {
    // d6.cov is a GATING-dimension cell (D6) marked CoveragePermitted; it resolves
    // to a coverage record → unmeasured on the evidence run → the gating claim
    // fails → experimental (A9). This is the lane_gating_cells fix: a
    // CoveragePermitted gating cell IS a gating requirement.
    let c = Builder::with_battery(coverage_gating_battery()).run(coverage_steps());
    assert_experimental(&c, Lane::Pi, "gating unmeasured or red");
    // NEGATIVE CONTROL: if lane_gating_cells reverts to Required-only, d6.cov drops
    // out of the gating set, the gating claim passes, and the verdict becomes beta
    // (d3.n/d1.g pass, coverage blocks GA) — this assertion then fails.
}

// ===========================================================================
// FIX-1 — v7 no-rollback + current/historical publication (T4 layer).
// ===========================================================================

/// A truncated prefix (`early`, anchored at an earlier snapshot) of a fuller
/// corpus (`late`, anchored at a later snapshot). `early` is byte-identical to
/// `late`'s prefix through the first snapshot — the exact shape of the rollback
/// exploit (re-anchor a truncated prefix and publish it as "current").
fn early_and_late_corpora() -> (Corpus, Corpus) {
    let base = || {
        vec![
            Step::ActivateContext,
            Step::Designate {
                lane: Lane::Pi,
                box_id: BOX,
                seat: "designation-seat (dddd4444)",
                kind: DesignationBasisKind::GreaterRunnability,
            },
            Step::Run(RunSpec::evidence("run-1")),
            Step::Run(RunSpec::evidence("run-2")),
            Step::Run(RunSpec::evidence("run-3")),
        ]
    };
    let mut early_steps = base();
    early_steps.push(Step::Snapshot("2026-07-15T12:00:00Z")); // snapshot 1 (earlier)
    let early = Builder::new().run(early_steps);

    let mut late_steps = base();
    late_steps.push(Step::Snapshot("2026-07-15T12:00:00Z")); // snapshot 1
    late_steps.push(Step::Run(RunSpec::evidence("run-4")));
    late_steps.push(Step::Snapshot("2026-07-16T12:00:00Z")); // snapshot 2 (later)
    let late = Builder::new().run(late_steps);
    (early, late)
}

/// The registrar attestation for a corpus's OWN latest snapshot — what a genuine
/// current publication of that corpus supplies (its anchor IS the registrar's
/// latest). Used both to make legitimate publications pass and, cross-supplied
/// (a corpus's anchor + a DIFFERENT corpus's attestation), to trigger the
/// currency check.
fn registrar_of(corpus: &Corpus) -> super::tier_doc::RegistrarAttestation {
    let (ord, snap) = corpus.journal.latest_snapshot().expect("anchored corpus");
    super::tier_doc::RegistrarAttestation {
        latest_ordinal: ord.get(),
        attested_head_digest: snap.artifact_index_head_digest.clone(),
        observed_at: snap.observed_at.clone(),
    }
}

#[test]
fn a_current_refresh_rolling_back_to_an_earlier_snapshot_is_invalid() {
    use super::tier_doc::{
        publish_tier_document, PublicationHistory, PublicationMode, PublishError,
    };
    let (early, late) = early_and_late_corpora();
    let mut history = PublicationHistory::default();
    // Publish the later corpus as CURRENT (registrar = its own latest) — establishes
    // the published high-water.
    publish_tier_document(
        &late,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&late),
        &mut history,
    )
    .unwrap();
    // A CURRENT refresh anchored at the EARLIER snapshot (the truncated prefix). We
    // supply early's OWN registrar attestation so obligation (2) passes and the
    // NO-ROLLBACK check is the one on trial: earlier ordinal < published floor →
    // INVALID (v7/R6-5).
    let err = publish_tier_document(
        &early,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&early),
        &mut history,
    )
    .unwrap_err();
    assert!(
        matches!(err, PublishError::RollBack { .. }),
        "expected RollBack, got {err:?}"
    );
    // NEGATIVE CONTROL: remove the no-rollback check and this call returns Ok —
    // `unwrap_err()` then panics, failing the fixture.
}

#[test]
fn a_current_publication_anchored_at_a_stale_intermediate_snapshot_is_invalid() {
    // Obligation (2), the FIX-round-2 item: a truncated corpus whose OWN latest is
    // an earlier snapshot, published "current" while the REGISTRAR attests a fresher
    // latest → NotAnchoredAtLatest. History is empty, so no-rollback cannot fire —
    // ONLY obligation (2) has teeth here (the favorable-intermediate case).
    use super::tier_doc::{
        publish_tier_document, PublicationHistory, PublicationMode, PublishError,
    };
    let (early, late) = early_and_late_corpora();
    let mut history = PublicationHistory::default();
    // early's anchor (its own snapshot 1) ≠ the registrar's true latest (late's
    // snapshot 2). No prior rows → no-rollback is silent.
    let err = publish_tier_document(
        &early,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&late),
        &mut history,
    )
    .unwrap_err();
    assert!(
        matches!(err, PublishError::NotAnchoredAtLatest { .. }),
        "expected NotAnchoredAtLatest (stale intermediate anchor), got {err:?}"
    );
    assert!(
        history.rows.is_empty(),
        "an invalid publication must not be recorded"
    );
    // The MATCHED case (anchor == registrar's latest) passes — a genuine current.
    publish_tier_document(
        &late,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&late),
        &mut history,
    )
    .unwrap();
    // NEGATIVE CONTROL: revert obligation (2) to the corpus-internal tautology (or
    // drop the check) → the first publish returns Ok, `unwrap_err()` panics.
}

#[test]
fn the_same_earlier_anchor_rendered_as_historical_is_legal() {
    use super::tier_doc::{publish_tier_document, PublicationHistory, PublicationMode};
    let (early, late) = early_and_late_corpora();
    let mut history = PublicationHistory::default();
    publish_tier_document(
        &late,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&late),
        &mut history,
    )
    .unwrap();
    // The SAME earlier anchor, published as an explicitly HISTORICAL rendering, is
    // legal (exempt from no-rollback AND from obligation (2) — it does not claim to
    // be current) and carries a visible HISTORICAL banner. The registrar attestation
    // is ignored for a historical rendering.
    let doc = publish_tier_document(
        &early,
        COMMIT,
        &P,
        PublicationMode::Historical,
        &registrar_of(&late),
        &mut history,
    )
    .unwrap();
    assert!(
        doc.contains("HISTORICAL RENDERING"),
        "historical rendering must be banner-marked"
    );
    assert!(doc.contains("Historical rendering as of"));
}

#[test]
fn a_forward_current_refresh_is_allowed_and_recorded() {
    use super::tier_doc::{publish_tier_document, PublicationHistory, PublicationMode};
    let (early, late) = early_and_late_corpora();
    let mut history = PublicationHistory::default();
    // early (earlier anchor) then late (later anchor) — strictly forward, both OK.
    // Each supplies its own registrar attestation (each is genuinely current at its
    // publication time).
    publish_tier_document(
        &early,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&early),
        &mut history,
    )
    .unwrap();
    publish_tier_document(
        &late,
        COMMIT,
        &P,
        PublicationMode::Current,
        &registrar_of(&late),
        &mut history,
    )
    .unwrap();
    assert_eq!(history.rows.len(), 2);
    assert!(history.rows[1].snapshot_ordinal > history.rows[0].snapshot_ordinal);
}

#[test]
fn an_unanchored_corpus_cannot_be_published() {
    use super::tier_doc::{
        publish_tier_document, PublicationHistory, PublicationMode, PublishError,
        RegistrarAttestation,
    };
    let mut c = minimal_corpus();
    mutate_journal(&mut c, |v| {
        v["entries"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["body"]["event"] != "publication-snapshot");
    });
    let mut history = PublicationHistory::default();
    let dummy = RegistrarAttestation {
        latest_ordinal: 0,
        attested_head_digest: super::ids::ArtifactDigest("sha256:none".into()),
        observed_at: String::new(),
    };
    let err = publish_tier_document(
        &c,
        COMMIT,
        &P,
        PublicationMode::Current,
        &dummy,
        &mut history,
    )
    .unwrap_err();
    assert_eq!(err, PublishError::NotAnchored);
}

// ===========================================================================
// RT-1 [HIGH] — A8 attribution-run-binding: a decoy rec.run must not launder a
// real run's fail. Post-fix (v9), the decoy record is malformed → INVALID.
// ===========================================================================

/// A malicious harness attribution: `disposition=Harness`, issued BY the target
/// run's executor, governing `observation`, but declaring a (possibly decoy)
/// `rec_run`. With a decoy run absent from the journal, A8's executor/coordinator
/// exclusion (keyed on rec.run) is skipped — the pre-v9 bypass.
fn harness_attr(
    id: &str,
    rec_run: &str,
    observation: ObservationId,
    issuer: &str,
) -> AttributionRecord {
    AttributionRecord {
        id: RecordId(id.into()),
        sequence: AttributionSeq::new(1),
        run: RunId(rec_run.into()),
        observations: vec![observation],
        disposition: Disposition::Harness,
        reason: ReasonCode::HarnessFlakeTiming,
        primary_evidence: vec!["ci-log".into()],
        issuer: issuer.into(),
        timestamp: "2026-07-15T00:00:00Z".into(),
        supersedes: vec![],
    }
}

#[test]
fn attr_executor_ruling_with_matching_run_is_rejected_by_authority() {
    // CONTROL: a harness ruling on run-3's fail, issued by run-3's executor, with a
    // MATCHING rec.run="run-3". v9 passes (obs run matches), but A8 authority
    // rejects it (issuer == the run's executor) → the fail STAYS product-counted.
    let mut c = corpus_with_evrun_fail(C_D3); // run-3 d3.n fails → 11/12
    let obs = ObservationId::derive(&RunId("run-3".into()), Lane::Pi, &CellId(C_D3.into()));
    c.attributions.push(harness_attr(
        "attr-ctl",
        "run-3",
        obs,
        "runner-1 (bbbb2222)",
    ));
    let comp = assert_reproduces(&c, Lane::Pi);
    // The fail is NOT laundered — still in the pool.
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 11,
            pool: 12
        })
    );
    assert_eq!(comp.verdict, TierVerdict::Beta);
}

#[test]
fn attr_decoy_run_cannot_launder_a_fail_out_of_the_pool() {
    // BYPASS (pre-v9): a decoy rec.run="run-decoy" (absent from the journal) lets
    // the executor-issued harness ruling on run-3's REAL observation slip past the
    // A8 exclusion → fail laundered. POST-v9: the record governs "run-3::…" but
    // declares run "run-decoy" → malformed → INVALID_EVIDENCE (fail STAYS counted).
    let mut c = corpus_with_evrun_fail(C_D3);
    let obs = ObservationId::derive(&RunId("run-3".into()), Lane::Pi, &CellId(C_D3.into()));
    c.attributions.push(harness_attr(
        "attr-decoy",
        "run-decoy",
        obs,
        "runner-1 (bbbb2222)",
    ));
    assert_invalid(&c, Lane::Pi, "decoy run cannot launder");
    // NEGATIVE CONTROL: remove v9 and this corpus computes beta (11/11 laundered)
    // instead of INVALID — the fixture then fails.
}

#[test]
fn attr_decoy_run_cannot_escalate_a_gating_fail_to_ga() {
    // The gating-escalation variant: a GATING fail on a NON-evidence window run
    // (run-1) normally breaks A5's "no product/pending gating fail anywhere in the
    // window" → experimental. A decoy-run harness ruling would launder it to
    // harness-removed (A5 exempts harness-removed gating fails elsewhere) → GA.
    // POST-v9: the decoy record is malformed → INVALID_EVIDENCE.
    let mut run1 = RunSpec::evidence("run-1");
    run1.overrides = vec![(Lane::Pi, C_D1, Cellcome::Fail("gating".into()))]; // d1.g gating
    let mut c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(run1),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let obs = ObservationId::derive(&RunId("run-1".into()), Lane::Pi, &CellId(C_D1.into()));
    c.attributions.push(harness_attr(
        "attr-esc",
        "run-decoy",
        obs,
        "runner-1 (bbbb2222)",
    ));
    assert_invalid(&c, Lane::Pi, "decoy run cannot launder");
    // NEGATIVE CONTROL: remove v9 → this corpus computes GA (gating fail laundered)
    // instead of INVALID — the fixture then fails.
}

// ===========================================================================
// RT-2 [MED] — artifact↔journal tuple-binding + fullness negatives.
// ===========================================================================

#[test]
fn neg_header_tuple_mismatches_its_journal_entry() {
    // A consumable artifact whose header tuple disagrees with its commissioning
    // entry (here: release_commit) → v3 rejects the whole corpus (N-1/R5-O3).
    let mut c = minimal_corpus();
    refile_run(&mut c, "run-3", |art| {
        art.header.tuple.release_commit = "commit-FORGED".into();
    });
    assert_invalid(&c, Lane::Pi, "header tuple mismatches its journal entry");
}

#[test]
fn neg_a_forged_tuple_candidate_alongside_clean_runs_yields_no_favorable_tier() {
    // Three clean runs (would be GA) + a fourth whose header tuple is forged. The
    // corpus is INVALID — an unverifiable corpus never yields a favorable tier
    // (T4), even though 3 clean runs exist.
    let mut c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Run(RunSpec::evidence("run-4")),
        Step::Snapshot(SNAP_TIME),
    ]);
    refile_run(&mut c, "run-4", |art| {
        art.header.tuple.box_id = BoxId("forged-box".into());
    });
    let comp = assert_reproduces(&c, Lane::Pi);
    assert!(
        matches!(comp.verdict, TierVerdict::InvalidEvidence { .. }),
        "a forged tuple must reject the whole corpus, not publish a favorable tier: {:?}",
        comp.verdict
    );
    assert_eq!(comp.tier, Tier::Experimental);
}

#[test]
fn neg_a_non_full_artifact_is_invalid() {
    // A consumable artifact missing a planned-domain cell (A2(d) fullness) → v5
    // rejects the corpus (never a quietly-excluded candidate).
    let mut c = minimal_corpus();
    refile_run(&mut c, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId(C_D5.into()));
        art.grid.remove(&key);
    });
    assert_invalid(&c, Lane::Pi, "missing a resolution for planned-domain cell");
}

// ===========================================================================
// RT-3 [MED] — A3 pooled-rate threshold arithmetic, decided by the rate.
// ===========================================================================

fn corpus_with_fail_on(run: &'static str, cell: &'static str) -> Corpus {
    let mut spec1 = RunSpec::evidence("run-1");
    let mut spec2 = RunSpec::evidence("run-2");
    let mut spec3 = RunSpec::evidence("run-3");
    for s in [&mut spec1, &mut spec2, &mut spec3] {
        if s.run == run {
            s.overrides
                .push((Lane::Pi, cell, Cellcome::Fail("injected".into())));
        }
    }
    Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(spec1),
        Step::Run(spec2),
        Step::Run(spec3),
        Step::Snapshot(SNAP_TIME),
    ])
}

#[test]
fn rate_demotes_ga_to_beta_via_the_pooled_rate_not_the_evidence_run() {
    // A non-gating fail on a NON-evidence window run (run-1). The evidence run is
    // all-green so ga_point_in_time PASSES — the ONLY thing keeping it off GA is
    // the pooled rate 11/12 = 91.6% < 95% (the 95 threshold, decided by the rate).
    let c = corpus_with_fail_on("run-1", C_D3);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 11,
            pool: 12
        })
    );
    assert_eq!(
        comp.verdict,
        TierVerdict::Beta,
        "91.6% must be beta (rate < 95), got {:?}",
        comp.verdict
    );
}

#[test]
fn rate_below_beta_is_experimental() {
    // Three non-gating fails (9/12 = 75% < 80%) with gating green → experimental
    // purely because the rate is below beta.
    let mut spec1 = RunSpec::evidence("run-1");
    spec1.overrides = vec![
        (Lane::Pi, C_D3, Cellcome::Fail("f".into())),
        (Lane::Pi, C_D5, Cellcome::Fail("f".into())),
    ];
    let mut spec2 = RunSpec::evidence("run-2");
    spec2.overrides = vec![(Lane::Pi, C_D3, Cellcome::Fail("f".into()))];
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(spec1),
        Step::Run(spec2),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 9,
            pool: 12
        })
    );
    assert!(
        matches!(comp.verdict, TierVerdict::Experimental { .. }),
        "75% must be experimental: {:?}",
        comp.verdict
    );
}

/// A 5-cell battery (2 gating, 3 non-gating, all Required for Pi) so a window pool
/// can be sized to hit an exact threshold percent (off-by-one guards).
fn rate_battery() -> Battery {
    use super::registry::{Applicability, Cell, Dimension};
    let cells = vec![
        Cell {
            id: CellId(C_D1.into()),
            dimension: Dimension::D1Lifecycle,
            title: "gating".into(),
        },
        Cell {
            id: CellId(C_D6.into()),
            dimension: Dimension::D6FailureHonesty,
            title: "gating".into(),
        },
        Cell {
            id: CellId(C_D3.into()),
            dimension: Dimension::D3BusyQueue,
            title: "n".into(),
        },
        Cell {
            id: CellId(C_D5.into()),
            dimension: Dimension::D5Transcript,
            title: "n".into(),
        },
        Cell {
            id: CellId("d3.n2".into()),
            dimension: Dimension::D3BusyQueue,
            title: "n".into(),
        },
    ];
    let mut applicability = BTreeMap::new();
    for c in [C_D1, C_D6, C_D3, C_D5, "d3.n2"] {
        applicability.insert(
            (Lane::Pi.id().to_string(), c.to_string()),
            Applicability::Required,
        );
    }
    Battery {
        cells,
        applicability,
        aggregation_version: AggregationVersion("agg-v1".into()),
    }
}

#[test]
fn rate_exactly_at_beta_threshold_is_beta() {
    // pool = 3 runs × 5 cells = 15; fail all 3 non-gating cells on run-1 → 12 pass
    // → EXACTLY 80.0%. gating green, evrun all-green. 80% >= 80 (beta) but < 95
    // (not GA) → beta. Off-by-one: `>=`→`>` would drop 80% below beta.
    let mut spec1 = RunSpec::evidence("run-1");
    spec1.overrides = vec![
        (Lane::Pi, C_D3, Cellcome::Fail("f".into())),
        (Lane::Pi, C_D5, Cellcome::Fail("f".into())),
        (Lane::Pi, "d3.n2", Cellcome::Fail("f".into())),
    ];
    let c = Builder::with_battery(rate_battery()).run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(spec1),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 12,
            pool: 15
        })
    ); // 80.0%
    assert_eq!(
        comp.verdict,
        TierVerdict::Beta,
        "exactly 80% must be beta: {:?}",
        comp.verdict
    );
}

#[test]
fn rate_exactly_at_ga_threshold_is_ga() {
    // W=4 (parameterized, same gated thresholds) → pool = 4 × 5 = 20; fail ONE
    // non-gating cell on run-1 → 19 pass → EXACTLY 95.0%. evrun (run-4) all-green,
    // gating green → GA. Off-by-one: `>=`→`>` would drop 95% below GA.
    let params = TierParams::custom(4, 80, 95, 24 * 60 * 60);
    let mut spec1 = RunSpec::evidence("run-1");
    spec1.overrides = vec![(Lane::Pi, C_D3, Cellcome::Fail("f".into()))];
    let c = Builder::with_battery(rate_battery()).run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(spec1),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Run(RunSpec::evidence("run-4")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let a = compute_tier(&c, Lane::Pi, COMMIT, &params);
    let json = serde_json::to_string(&c).unwrap();
    let b = compute_tier(
        &serde_json::from_str::<Corpus>(&json).unwrap(),
        Lane::Pi,
        COMMIT,
        &params,
    );
    assert_eq!(bytes(&a), bytes(&b));
    assert_eq!(
        a.rate,
        Some(RateValue::Rate {
            passes: 19,
            pool: 20
        })
    ); // 95.0%
    assert_eq!(
        a.verdict,
        TierVerdict::Ga,
        "exactly 95% must be GA: {:?}",
        a.verdict
    );
}

// ===========================================================================
// Declared-gap honesty (QS-2) — corpus-wide, not evrun-only (fix-r6 audit).
// ===========================================================================

#[test]
fn a_declared_blocked_gap_is_beta_but_an_empty_reason_is_invalid_corpus_wide() {
    // A non-gating cell resolved BLOCKED with a valid reason is beta-compatible
    // (blocked is a declared gap). Forge its reason empty → INVALID_EVIDENCE via v5
    // (an undeclared not-run invalidates the grid, QS-2). Post fix-r6 this is
    // checked CORPUS-WIDE — beta_pt's evrun-only reason check is now
    // defense-in-depth. (The would-be inflation: relabel a FAIL as an empty-reason
    // BLOCKED on a non-evidence window run to launder it out of the A3 pool.)
    let mut run3 = RunSpec::evidence("run-3");
    run3.overrides = vec![(Lane::Pi, C_D3, Cellcome::Blocked)];
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(RunSpec::evidence("run-1")),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(run3),
        Step::Snapshot(SNAP_TIME),
    ]);
    // Twin: a declared (non-empty reason) Blocked gap on the evidence run → beta.
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.verdict,
        TierVerdict::Beta,
        "declared blocked gap → beta: {:?}",
        comp.verdict
    );

    // Forge the blocked reason empty → INVALID (v5, corpus-wide QS-2).
    let mut c2 = c.clone();
    refile_run(&mut c2, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId(C_D3.into()));
        if let Some(super::evidence::CellResolution::Observed(o)) = art.grid.get_mut(&key) {
            o.outcome = Outcome::blocked("", "reentry");
        }
    });
    assert_invalid(&c2, Lane::Pi, "undeclared not-run");
}

#[test]
fn an_empty_reason_blocked_on_a_non_evidence_window_run_is_rejected() {
    // The inflation vector the corpus-wide check closes: an empty-reason BLOCKED on
    // a NON-evidence window run (run-1) — evrun-only beta_pt never saw it. INVALID.
    let mut c = minimal_corpus();
    refile_run(&mut c, "run-1", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId(C_D3.into()));
        if let Some(super::evidence::CellResolution::Observed(o)) = art.grid.get_mut(&key) {
            o.outcome = Outcome::blocked("", ""); // empty reason AND re-entry
        }
    });
    assert_invalid(&c, Lane::Pi, "undeclared not-run");
    // NEGATIVE CONTROL: the corpus-wide v5 declared-reason check removed → the
    // empty-reason blocked launders run-1's cell out of the pool (rate rises) and
    // the corpus computes a tier instead of INVALID.
}

// ===========================================================================
// RT-ASSESS — A9 over-strictness fix: a valid quiet withdrawal issued BEFORE the
// would-be promotion ordinal is now accepted (was a false-REJECT).
// ===========================================================================

#[test]
fn a_quiet_withdrawal_before_the_wth_run_retires_the_pending() {
    // Pending devbox; devbox-1 and devbox-2 complete; a quiet, outcome-independent
    // withdrawal is issued (devbox-3 not yet commissioned → quiet); THEN devbox-3
    // completes. Eager promotion (the old bug) would derive at devbox-3's terminal
    // and reject the earlier withdrawal as "no outstanding pending." The fix: a
    // withdrawal in (boundary, promotion] preempts the promotion → the pending is
    // legitimately retired → the corpus is VALID (Pi = experimental on lima, its
    // active box, which has no runs), NOT invalid.
    let mut devbox1 = RunSpec::evidence("devbox-1");
    devbox1.box_id = "devbox".into();
    let mut devbox2 = RunSpec::evidence("devbox-2");
    devbox2.box_id = "devbox".into();
    let mut devbox3 = RunSpec::evidence("devbox-3");
    devbox3.box_id = "devbox".into();
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(devbox1),
        Step::Run(devbox2),
        Step::Withdraw {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::DurablyUnreachable,
        },
        Step::Run(devbox3),
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert!(
        matches!(comp.verdict, TierVerdict::Experimental { .. }),
        "a valid pre-promotion quiet withdrawal must be accepted (not INVALID): {:?}",
        comp.verdict
    );
    assert_eq!(comp.pending_box, None, "the withdrawal retired the pending");
    assert_eq!(
        comp.active_box.as_deref(),
        Some("lima"),
        "active box unchanged by the withdrawn pending"
    );
    assert_eq!(
        comp.promotion_ordinal, None,
        "no promotion — the withdrawal preempted it"
    );
    // NEGATIVE CONTROL: without the preempt check, eager promotion fires at devbox-3
    // and the withdrawal is rejected → the corpus computes INVALID_EVIDENCE, so the
    // Experimental assertion fails.
}

// ===========================================================================
// F-RT1b [HIGH] — v9 delimiter-injection: "::" inside a run id defeated the
// starts_with prefix test. v0 removes the ambiguity at the id-charset source.
// ===========================================================================

#[test]
fn attr_double_colon_run_id_defeats_v9_prefix_and_is_rejected() {
    // The exact exploit: a REAL window run whose id is "X::Y" (contains the
    // reserved "::") carrying a non-gating fail, plus a decoy rec.run="X" (a
    // "::"-delimited prefix segment, absent from the journal) issued by that run's
    // OWN executor. Pre-fix: "X::Y::pi::d3.n".starts_with("X::") = TRUE → v9 passes
    // → the fail launders (beta→GA). POST-FIX: v0 rejects the run id "X::Y" →
    // INVALID_EVIDENCE, so the fail can never be laundered.
    let mut runxy = RunSpec::evidence("X::Y");
    runxy.overrides = vec![(Lane::Pi, C_D3, Cellcome::Fail("non-gating".into()))];
    let mut c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Run(runxy),
        Step::Run(RunSpec::evidence("run-2")),
        Step::Run(RunSpec::evidence("run-3")),
        Step::Snapshot(SNAP_TIME),
    ]);
    let obs = ObservationId::derive(&RunId("X::Y".into()), Lane::Pi, &CellId(C_D3.into()));
    c.attributions
        .push(harness_attr("attr-inj", "X", obs, "runner-1 (bbbb2222)"));
    assert_invalid(&c, Lane::Pi, "contains the reserved delimiter");
    // NEGATIVE CONTROL: with v0 (and the v9 local guard) removed, this corpus
    // launders the fail and computes GA (11/11) instead of INVALID.
}

#[test]
fn attr_double_colon_decoy_rec_run_is_rejected() {
    // The symmetric variant: a "::"-free real run "run-3" with a fail, and a decoy
    // rec.run="run-3::pi" (contains "::") — "run-3::pi::d3.n".starts_with(
    // "run-3::pi::") = TRUE would bypass. POST-FIX: v0/v9 reject the "::"-bearing
    // rec.run → INVALID.
    let mut c = corpus_with_evrun_fail(C_D3); // run-3 d3.n fails
    let obs = ObservationId::derive(&RunId("run-3".into()), Lane::Pi, &CellId(C_D3.into()));
    c.attributions.push(harness_attr(
        "attr-sym",
        "run-3::pi",
        obs,
        "runner-1 (bbbb2222)",
    ));
    assert_invalid(&c, Lane::Pi, "reserved delimiter");
}

#[test]
fn attr_colon_free_honest_attribution_still_validates_cleanly() {
    // HONEST TWIN: the same shape with "::"-free ids and a TRUTHFUL rec.run (issued
    // by a neutral seat) validates cleanly and removes the fail — v0 does not
    // over-reject legitimate corpora.
    let mut c = corpus_with_evrun_fail(C_D3); // run-3 d3.n fails → 11/12
    let obs = ObservationId::derive(&RunId("run-3".into()), Lane::Pi, &CellId(C_D3.into()));
    c.attributions.push(harness_attr(
        "attr-honest",
        "run-3",
        obs,
        "attributor (99998888)",
    ));
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.rate,
        Some(RateValue::Rate {
            passes: 11,
            pool: 11
        }),
        "valid harness removal → 11/11"
    );
    assert_eq!(comp.verdict, TierVerdict::Beta);
}

// ===========================================================================
// RT-5 [MED] — A9 withdrawal box_id must name the pending it retires.
// ===========================================================================

#[test]
fn neg_withdrawal_naming_an_unrelated_box_is_rejected() {
    // A withdrawal with a valid outcome-independent basis + quiet window, but
    // naming an UNRELATED box (not the outstanding pending), must NOT retire the
    // pending — that would suppress a legitimate promotion / keep a favorable
    // incumbent. box_id unbound to the pending → INVALID_EVIDENCE.
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Withdraw {
            lane: Lane::Pi,
            box_id: "unrelated-box", // ≠ the outstanding pending "devbox"
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::DurablyUnreachable,
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    assert_invalid(&c, Lane::Pi, "must name the pending box it retires");
    // NEGATIVE CONTROL: remove the box_id binding → the unrelated-box withdrawal
    // retires the pending → Pi computes experimental (not INVALID) → fixture fails.
}

#[test]
fn a_withdrawal_naming_the_pending_box_retires_it_cleanly() {
    // HONEST TWIN: the same withdrawal correctly naming the pending box ("devbox")
    // retires it with no over-rejection.
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Withdraw {
            lane: Lane::Pi,
            box_id: "devbox", // correctly names the pending
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::DurablyUnreachable,
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert!(
        matches!(comp.verdict, TierVerdict::Experimental { .. }),
        "clean retire: {:?}",
        comp.verdict
    );
    assert_eq!(comp.pending_box, None, "the pending was retired");
}

// ===========================================================================
// Free-floating-field audit: inner record identity bound to grid position (v5).
// ===========================================================================

#[test]
fn neg_decoy_observation_id_is_rejected() {
    // A Fail observation carrying a decoy o.id (not its position-derived id) would
    // let an attribution for that GHOST id launder it (the head lookup keys on
    // o.id, the pool on the grid key). v5 binds o.id == derive(run, lane, cell) →
    // INVALID.
    let mut c = corpus_with_evrun_fail(C_D3); // run-3 d3.n = Fail
    refile_run(&mut c, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId(C_D3.into()));
        if let Some(super::evidence::CellResolution::Observed(o)) = art.grid.get_mut(&key) {
            o.id = ObservationId("run-3::pi::GHOST".into());
        }
    });
    assert_invalid(&c, Lane::Pi, "decoy observation id");
}

#[test]
fn neg_coverage_record_with_a_mismatched_cell_is_rejected() {
    // A coverage record whose internal cell ≠ its grid-key cell (a floating
    // identity that could misattribute provenance in T4) → v5 INVALID.
    let mut c = Builder::with_battery(coverage_battery()).run(coverage_steps());
    refile_run(&mut c, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId("d5.cov".into()));
        if let Some(super::evidence::CellResolution::Coverage(cov)) = art.grid.get_mut(&key) {
            cov.cell = CellId("d3.n".into()); // ≠ the grid-key cell d5.cov
        }
    });
    assert_invalid(&c, Lane::Pi, "coverage record");
}

// ===========================================================================
// fix-r7 RT-6 — (re-)designation/withdrawal basis is a TYPED closed enum.
// ===========================================================================

/// Null out the typed `basis_kind` of the lane's designation event of `kind`
/// (and optionally set an outcome-dependent DESCRIPTIVE `policy_basis`) — the only
/// way to express an outcome-dependent basis under the typed model (there is no
/// outcome-dependent kind). Reproduces the RT-6 bypass attempts: the free text is
/// not machine-checked, so a null kind is the actual state → INVALID.
fn null_basis_kind(corpus: &mut Corpus, kind: &str, dishonest_text: &str) {
    mutate_journal(corpus, |v| {
        for e in v["entries"].as_array_mut().unwrap() {
            if e["body"]["event"] == "designation" && e["body"]["kind"] == kind {
                e["body"]["basis_kind"] = serde_json::Value::Null;
                e["body"]["policy_basis"] = serde_json::Value::String(dishonest_text.into());
            }
        }
    });
}

#[test]
fn neg_pending_replacement_with_no_valid_basis_kind_is_rejected() {
    // RT-6: an outcome-dependent re-designation has NO valid kind. The old
    // word-denylist could be evaded by synonyms ("aced", "outperformed"), spacing
    // ("p a s s e d"), or homoglyphs (Cyrillic "раss"). Under the typed enum the
    // descriptive free text is NOT machine-checked; the actor can only omit/null
    // the kind → INVALID. The bypass phrasings in the descriptive text do not help.
    let mut c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    null_basis_kind(
        &mut c,
        "pending-replacement",
        "pick devbox — its window aced the runs; раss-heavy; p a s s e d best", // all 3 bypasses
    );
    assert_invalid(
        &c,
        Lane::Pi,
        "not an admissible outcome-independent re-designation kind",
    );
    // NEGATIVE CONTROL: loosen the typed check to accept any kind → the null-kind
    // (outcome-dependent) re-designation is accepted → experimental not INVALID.
}

#[test]
fn each_valid_basis_kind_validates_on_its_arm() {
    // HONEST TWINS. Pending accepts RunnabilityReduced (a leave-reason):
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::RunnabilityReduced,
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp = assert_reproduces(&c, Lane::Pi);
    assert_eq!(
        comp.pending_box.as_deref(),
        Some("devbox"),
        "valid pending stands: {:?}",
        comp.verdict
    );

    // Withdraw accepts Decommissioned (a leave-reason):
    let c2 = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Withdraw {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::Decommissioned,
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    let comp2 = assert_reproduces(&c2, Lane::Pi);
    assert_eq!(
        comp2.pending_box, None,
        "the valid withdrawal retired the pending: {:?}",
        comp2.verdict
    );
}

#[test]
fn a_withdrawal_citing_a_toward_kind_is_rejected_cross_arm() {
    // Cross-arm: GreaterRunnability is a "toward" kind — inadmissible for a
    // WITHDRAWAL (you do not withdraw because some box has greater runnability).
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Pending {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability,
        },
        Step::Withdraw {
            lane: Lane::Pi,
            box_id: "devbox",
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::GreaterRunnability, // inadmissible for withdrawal
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    assert_invalid(
        &c,
        Lane::Pi,
        "not an admissible outcome-independent withdrawal kind",
    );
}

#[test]
fn an_initial_designation_citing_a_leave_kind_is_rejected_cross_arm() {
    // Cross-arm: Decommissioned is a leave-reason — inadmissible for an INITIAL
    // designation (which selects a box for maximal runnability).
    let c = Builder::new().run(vec![
        Step::ActivateContext,
        Step::Designate {
            lane: Lane::Pi,
            box_id: BOX,
            seat: "designation-seat (dddd4444)",
            kind: DesignationBasisKind::Decommissioned, // inadmissible for initial designate
        },
        Step::Snapshot(SNAP_TIME),
    ]);
    assert_invalid(
        &c,
        Lane::Pi,
        "not an admissible outcome-independent designation kind",
    );
}

#[test]
fn neg_coverage_record_with_an_empty_reason_is_rejected() {
    // Coverage well-formedness (C-1 validates on write; a forged corpus consumes it
    // unvalidated) is now re-checked corpus-wide (v5): an empty why-not-here is an
    // undeclared gap → INVALID.
    let mut c = Builder::with_battery(coverage_battery()).run(coverage_steps());
    refile_run(&mut c, "run-3", |art| {
        let key = RunArtifact::cell_key(Lane::Pi, &CellId("d5.cov".into()));
        if let Some(super::evidence::CellResolution::Coverage(cov)) = art.grid.get_mut(&key) {
            cov.why_not_here = "   ".into(); // empty (malformed)
        }
    });
    assert_invalid(&c, Lane::Pi, "malformed");
}
