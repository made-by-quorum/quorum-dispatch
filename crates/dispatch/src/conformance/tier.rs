//! C-3 — the tier scheme, computation, and citation (the aggregation semantics
//! A1–A10). This module realizes **exactly** the A10 decision procedure
//! preregistered in release plan R9 (`### C-3`). A10 is **normative**; the prose
//! elsewhere is commentary — where they could differ, A10 governs. Every stage,
//! validation check (v1–v8), and predicate below cites its A-clause / v-number so
//! the doctrine-alignment oracle can check the code against the pseudocode line
//! by line.
//!
//! **The tier function is TOTAL** (QS-4): `compute_tier` maps *every* input
//! corpus — well-formed or not — to exactly one [`TierComputation`]. A corpus
//! that fails any staged validation resolves to the single normative
//! [`TierVerdict::InvalidEvidence`] (an experimental row carrying the failed
//! checks); it can never emit beta or GA. No case is undefined.
//!
//! **Deterministic + reproducible** (QS-4): the inputs are total and explicit —
//! nothing is read from ambient state. The release commit is the one context
//! parameter; the manifest digest and aggregation version are DERIVED from it
//! (via [`Corpus::commits`]); the designation state is DERIVED by replaying the
//! authority journal; promotion is replay arithmetic, never read from an event.
//! Two readers of the same corpus + release commit compute a byte-identical
//! result.
//!
//! **Layering.** C-1 (schema) made each artifact / record / chain / journal
//! structurally honest and carries every operand this function reads. C-3 runs
//! the *corpus-scope* validate-then-compute. This module does not execute cells
//! (that is the harness) — it consumes recorded corpora.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::attribution::{resolve_heads, AttributionRecord, Disposition};
use super::evidence::{CellResolution, Outcome, RunArtifact, RunMode};
use super::ids::{
    AggregationVersion, ArtifactDigest, CellId, JournalOrdinal, Lane, ManifestDigest,
    ObservationId, RunId, RunKind, RunSequence,
};
use super::journal::{
    AuthorityJournal, DesignationBasisKind, DesignationEvent, DesignationKind, JournalBody,
    TerminalState,
};
use super::registry::{Applicability, Battery};

// ===========================================================================
// Gated parameters (F-5) — LOCKED per Pete ruling 01KXGVD5ZB ("as proposed, no
// changes"): W=3, beta≥0.80, ga≥0.95, delta=24h. A single source so an amendment
// propagates everywhere. Thresholds are held as integer percents so tier
// comparison is exact integer arithmetic — never a float rounding artifact.
// ===========================================================================

/// The gated aggregation parameters. Built as fixed constants per the ratified
/// thresholds; the `custom` constructor exists only so the doctrine oracle can
/// exercise the parameterization (F-5) — production uses [`TierParams::gated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierParams {
    /// W — the window size (exactly 3, gated).
    pub window: usize,
    /// beta threshold as an integer percent (80 = 0.80).
    pub beta_pct: u64,
    /// GA threshold as an integer percent (95 = 0.95).
    pub ga_pct: u64,
    /// delta — the run-liveness suppression deadline, in seconds (24h).
    pub delta_secs: i64,
}

impl TierParams {
    /// The gated production values (F-5): W=3, beta=0.80, ga=0.95, delta=24h.
    pub const fn gated() -> Self {
        TierParams {
            window: 3,
            beta_pct: 80,
            ga_pct: 95,
            delta_secs: 24 * 60 * 60,
        }
    }

    /// A parameterized set (F-5) — for exercising an amended W / delta.
    pub const fn custom(window: usize, beta_pct: u64, ga_pct: u64, delta_secs: i64) -> Self {
        TierParams {
            window,
            beta_pct,
            ga_pct,
            delta_secs,
        }
    }
}

impl Default for TierParams {
    fn default() -> Self {
        Self::gated()
    }
}

// ===========================================================================
// The corpus A10 consumes: the authenticated complete artifact index + the
// authority journal + the attribution records + the per-commit context (manifest
// + aggregation version derived from the release commit) + the A8 role registry.
// ===========================================================================

/// The commit-derived context (F-3): the battery (manifest) at a release commit.
/// A10 derives the manifest digest and aggregation version from the release
/// commit — never from ambient state. In the real dispatch tree the manifest and
/// scheme-version artifacts live in the pinned tree; here the corpus carries the
/// battery at each commit, so `manifest_at(commit)` and `agg_version_at(commit)`
/// are pure lookups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitContext {
    pub battery: Battery,
}

/// One entry in the authenticated complete artifact index: an artifact plus the
/// content digest it is filed under. The digest is what a completion event cites
/// (R6-1); v2 authenticates it (the filed digest must equal the artifact's own
/// content digest) so a mis-filed artifact cannot masquerade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedArtifact {
    pub digest: ArtifactDigest,
    pub artifact: RunArtifact,
}

/// The authenticated complete artifact index (R6-5), read at the snapshot-attested
/// head. Consumability is BY CITATION (R6-1): an index entry cited by no
/// completion event in the anchored prefix is corpus-invisible — never a
/// candidate and never a failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIndex {
    pub artifacts: Vec<IndexedArtifact>,
}

impl ArtifactIndex {
    /// The index head digest (R6-5): a stable SHA-256 over the canonical set of
    /// filed digests. Order-independent (digests are sorted first), so two
    /// readers derive the same head; v7 requires it to equal the anchoring
    /// snapshot's attested digest.
    pub fn head_digest(&self) -> ArtifactDigest {
        use sha2::{Digest, Sha256};
        let mut digs: Vec<&str> = self.artifacts.iter().map(|a| a.digest.0.as_str()).collect();
        digs.sort_unstable();
        digs.dedup();
        let mut hasher = Sha256::new();
        for d in digs {
            hasher.update(d.as_bytes());
            hasher.update([0u8]);
        }
        ArtifactDigest(format!("sha256:{:x}", hasher.finalize()))
    }

    /// The artifact filed under `digest`, if present.
    pub fn get(&self, digest: &ArtifactDigest) -> Option<&RunArtifact> {
        self.artifacts
            .iter()
            .find(|a| &a.digest == digest)
            .map(|a| &a.artifact)
    }
}

/// The A8 role registry: lane owners and cell authors, the roles the attribution
/// seat MUST NOT be (in addition to the run's executor and coordinator, which are
/// journal-derived). Optional per lane — a fixture that exercises the lane-owner /
/// cell-author exclusion supplies them; the executor/coordinator exclusions are
/// always enforced from the journal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRegistry {
    /// lane → the lane owner's identity.
    pub lane_owners: BTreeMap<Lane, String>,
    /// lane → the set of identities that authored a cell in the lane.
    pub cell_authors: BTreeMap<Lane, BTreeSet<String>>,
}

/// The complete corpus A10 validates and computes over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corpus {
    pub journal: AuthorityJournal,
    pub index: ArtifactIndex,
    pub attributions: Vec<AttributionRecord>,
    /// release_commit → the context (manifest/battery) derived from it (F-3).
    pub commits: BTreeMap<String, CommitContext>,
    pub roles: RoleRegistry,
}

// ===========================================================================
// The output: a tier row (the published tier), its verdict, its citation, and
// the operands a fresh reader reproduces. Serializable + Eq so two computations
// byte-match and negative corpora assert an identical result.
// ===========================================================================

/// The three tiers, ordered experimental < beta < GA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Experimental,
    Beta,
    Ga,
}

/// A pooled rate (A3): passes over pool size, or UNAVAILABLE — a distinct value,
/// not 0% and not 100%, that fails every threshold comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RateValue {
    /// passes ÷ (passes+fails); carried as the integer pair so equality is exact.
    Rate { passes: u64, pool: u64 },
    /// Zero pooled denominator (A3): fails every threshold.
    Unavailable,
}

impl RateValue {
    /// `passes/pool >= pct/100`, exact integer arithmetic. UNAVAILABLE fails
    /// every comparison (A3).
    fn at_least(&self, pct: u64) -> bool {
        match self {
            RateValue::Rate { passes, pool } => passes * 100 >= pct * pool,
            RateValue::Unavailable => false,
        }
    }
}

/// The normative verdict. The published [`Tier`] is derived from it: every
/// non-beta/GA verdict is an EXPERIMENTAL row (A10). `InvalidEvidence` is the
/// single normative result for a corpus that fails staged validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum TierVerdict {
    /// Staged validation failed (N-3): an experimental row carrying the failed
    /// checks. Can never emit beta or GA.
    InvalidEvidence { failed_checks: Vec<String> },
    /// Experimental with a named, honest label (window not accumulated, gating
    /// unmeasured/red, below beta, …).
    Experimental { label: String },
    /// In-flight evidence unresolved past the deadline (R6-1) — an experimental
    /// row citing the unresolved entries.
    Suppressed { label: String, entries: Vec<RunId> },
    /// beta: gating green + aggregate ≥ beta over the window + point-in-time.
    Beta,
    /// GA: all applicable cells green on the evidence run + aggregate ≥ ga +
    /// evidence at the current release commit.
    Ga,
}

impl TierVerdict {
    /// The published tier the verdict yields (A10): everything but beta/GA is
    /// experimental.
    pub fn tier(&self) -> Tier {
        match self {
            TierVerdict::Ga => Tier::Ga,
            TierVerdict::Beta => Tier::Beta,
            _ => Tier::Experimental,
        }
    }
}

/// The six context inputs every published tier row cites (F-3/N-2/R5-O4/R6-5).
/// Uncited claims are invalid by construction; a claim whose evidence commit is
/// not the current release commit renders stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub release_commit: String,
    pub manifest_digest: Option<ManifestDigest>,
    pub aggregation_version: Option<AggregationVersion>,
    /// The authority-journal version: content digest + high-water sequence (N-2).
    pub journal_content_digest: ArtifactDigest,
    pub journal_high_water: u64,
    /// The artifact-index head digest the corpus was read at (R5-O4).
    pub index_head_digest: ArtifactDigest,
    /// The anchoring publication-snapshot: ordinal + attested time (R6-5). Absent
    /// only when the corpus is not anchored at a snapshot (itself a v7 failure).
    pub snapshot_ordinal: Option<u64>,
    pub snapshot_attested_time: Option<String>,
}

/// The full computed tier row: the verdict, the published tier, the citation, and
/// the reproduced operands (rate, window, evidence run, designation state,
/// staleness). Byte-matchable across two computations of the same corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierComputation {
    pub lane: Lane,
    pub verdict: TierVerdict,
    pub tier: Tier,
    pub citation: Citation,
    /// The pooled rate over the window (None when validation stopped before the
    /// window was computed).
    pub rate: Option<RateValue>,
    /// The evidence run (the most recent eligible run), when a window formed.
    pub evidence_run: Option<RunId>,
    /// The selected window, in (sequence, run-id) order, when one formed.
    pub window: Vec<RunId>,
    /// The active designated evidence box for the lane, when replay derived one.
    pub active_box: Option<String>,
    /// A pending replacement box + its effective-after boundary ordinal, when one
    /// stands (A9).
    pub pending_box: Option<String>,
    pub pending_boundary: Option<u64>,
    /// The derived promotion ordinal, when a pending replacement has promoted
    /// (replay arithmetic, never read from an event — R5-O5).
    pub promotion_ordinal: Option<u64>,
    /// The count of eligible pending-box runs accumulated toward promotion (0..W)
    /// — A9's honest accumulation state, surfaced for T4.
    pub accumulation: u64,
    /// The full append-only designation history for the lane (T4 publishes it, not
    /// just the current state).
    pub designation_history: Vec<DesignationHistoryEntry>,
    /// Staleness (T2 citation-decay): the evidence run's release commit is not the
    /// current release commit. A beta claim can stand while stale; GA cannot.
    pub stale: bool,
}

// ===========================================================================
// The staged, total tier function (A10). Every stage cites its R5-O2 step.
// ===========================================================================

/// Compute the tier of `lane` from `corpus` at `release_commit` under `params`
/// (A10). Total: every corpus maps to exactly one [`TierComputation`].
pub fn compute_tier(
    corpus: &Corpus,
    lane: Lane,
    release_commit: &str,
    params: &TierParams,
) -> TierComputation {
    let citation = build_citation(corpus, release_commit);

    // manifest_at(release_commit) / agg_version_at(release_commit) — derived, not
    // ambient (F-3). A missing context artifact at the release commit is a v6
    // failure; without the battery we cannot compute a domain, so we short to
    // INVALID_EVIDENCE with that reason.
    let battery = match corpus.commits.get(release_commit) {
        Some(ctx) => &ctx.battery,
        None => {
            return invalid(
                lane,
                citation,
                vec![format!(
                    "v6: no manifest/scheme-version context artifact exists at release commit {release_commit:?}"
                )],
            );
        }
    };
    let manifest_digest = battery.manifest_digest();
    let agg_version = battery.aggregation_version.clone();

    // ---- STAGE 1: cutoff-independent invariants (R5-O2 step 1) --------------
    // Designation replay is computed here so v6 can check its legality; the
    // result is reused by stages 2/3 (the caller picks nothing).
    let replay = replay_designations(
        &corpus.journal,
        lane,
        &manifest_digest,
        &agg_version,
        params,
    );
    let mut failed = Vec::new();
    failed.extend(v0_id_wellformedness(corpus));
    failed.extend(v1_journal_integrity(&corpus.journal));
    failed.extend(v2_cardinality_matrix(corpus));
    failed.extend(v3_issuance_and_tuple_binding(corpus));
    failed.extend(v5_exact_observation_domain(corpus, battery));
    failed.extend(v6_context_epoch_and_designation(
        corpus,
        lane,
        &manifest_digest,
        &agg_version,
        &replay,
    ));
    failed.extend(v7_publication_anchoring(corpus));
    failed.extend(v8_launch_nonce_binding(corpus));
    failed.extend(v9_attribution_run_binding(corpus));
    if !failed.is_empty() {
        failed.sort();
        failed.dedup();
        return invalid(lane, citation, failed);
    }

    // ---- STAGE 2: designation-epoch replay (R5-O2 step 2) ------------------
    // Already computed as `replay`; v6 proved it legal. `box` is the ACTIVE
    // designated evidence box the published tier reads.
    let active_box = match &replay.active_box {
        Some(b) => b.clone(),
        None => {
            // No designation for the lane → no designated evidence box → the
            // lane has no official window (experimental, honest label).
            return experimental(
                lane,
                citation,
                &replay,
                "window not accumulated: lane has no designated evidence box",
            );
        }
    };

    // The anchoring snapshot's attested time is the computation's "now" for the
    // run-liveness deadline (R6-1). Absent only on a v7 failure, already caught.
    let snapshot_now = corpus
        .journal
        .latest_snapshot()
        .map(|(_, s)| s.observed_at.clone());

    // ---- STAGE 3: the NAMED prospective candidate cutoff (R5-O2 steps 3-4) --
    let candidates = eligible_candidates(
        corpus,
        lane,
        &active_box,
        &manifest_digest,
        &agg_version,
        replay.active_boundary,
        battery,
    );
    if candidates.is_empty() {
        // The zero-candidate case, defined (R5-O2).
        return experimental(
            lane,
            citation,
            &replay,
            "window not accumulated: no eligible completed runs",
        );
    }
    let candidate_cutoff = candidates.iter().map(|c| c.sequence.get()).max().unwrap();

    // ---- STAGE 4: terminal completeness through the cutoff (fail-closed) ----
    let v4 = v4_terminal_complete(corpus, lane, &active_box, candidate_cutoff);
    if !v4.is_empty() {
        return invalid(lane, citation, v4);
    }

    // ---- STAGE 5: run-liveness suppression, eligibility, window, computation -
    // Run-liveness suppression (R6-1): a started-open evidence entry for this
    // (lane, box) whose run-start attested time is more than delta older than the
    // anchoring snapshot suppresses every window-dependent claim.
    if let Some(now) = &snapshot_now {
        let stale_entries = stale_in_flight(corpus, lane, &active_box, now, params.delta_secs);
        if !stale_entries.is_empty() {
            return TierComputation {
                lane,
                tier: Tier::Experimental,
                verdict: TierVerdict::Suppressed {
                    label: "in-flight evidence unresolved past deadline".into(),
                    entries: stale_entries.clone(),
                },
                citation,
                rate: None,
                evidence_run: None,
                window: Vec::new(),
                active_box: Some(active_box),
                pending_box: replay.pending_box.clone(),
                pending_boundary: replay.boundary_ordinal(),
                promotion_ordinal: replay.promotion_ordinal,
                accumulation: replay.accumulation,
                designation_history: replay.history.clone(),
                stale: false,
            };
        }
    }

    // eligibility == candidacy (the same named rules; completeness now proven
    // through the cutoff). Order by (run_sequence, run_id) — F-3, NEVER timestamp.
    let mut window_all = candidates;
    window_all.sort_by(|a, b| (a.sequence.get(), &a.run.0).cmp(&(b.sequence.get(), &b.run.0)));
    if window_all.len() < params.window {
        return experimental(lane, citation, &replay, "window not accumulated");
    }
    let window: Vec<&Candidate> = window_all.iter().rev().take(params.window).rev().collect();
    let evrun = *window.last().unwrap();
    let window_ids: Vec<RunId> = window.iter().map(|c| c.run.clone()).collect();

    // Per-observation classification (A1/A3/A6/A8): read each failed
    // observation's UNIQUE ACTIVE HEAD from the authority-VALID records only.
    let heads = resolve_valid_heads(corpus, lane);

    // Pooled rate (A3): pooled over the window's (lane, cell) observations —
    // lane projection (R5-O3). Fails = product + pending. Harness-removed out of
    // num AND denom.
    let mut passes = 0u64;
    let mut pool = 0u64;
    for cand in &window {
        for (cell, class) in classify_lane(&cand.artifact, lane, &heads) {
            let _ = cell;
            match class {
                ObsClass::Pass => {
                    passes += 1;
                    pool += 1;
                }
                ObsClass::FailProduct | ObsClass::FailPending => {
                    pool += 1;
                }
                ObsClass::FailHarnessRemoved
                | ObsClass::Blocked
                | ObsClass::Na
                | ObsClass::Coverage => {}
            }
        }
    }
    let rate = if pool > 0 {
        RateValue::Rate { passes, pool }
    } else {
        RateValue::Unavailable
    };

    // Gating (A5): every gating cell PASS on the evidence run, and no product/
    // pending gating fail anywhere in the window.
    let gating_cells = lane_gating_cells(battery, lane);
    let gating_ok = evaluate_gating(&window, evrun, lane, &gating_cells, &heads);

    // Point-in-time predicates (A4/A7).
    let beta_pt = beta_point_in_time(evrun, lane, battery);
    let ga_pt = ga_point_in_time(evrun, lane, battery, release_commit);
    let stale = evrun.artifact.header.tuple.release_commit != release_commit;

    let verdict = if !gating_ok {
        TierVerdict::Experimental {
            label: "gating unmeasured or red".into(),
        }
    } else if ga_pt && rate.at_least(params.ga_pct) {
        TierVerdict::Ga
    } else if beta_pt && rate.at_least(params.beta_pct) {
        TierVerdict::Beta
    } else {
        TierVerdict::Experimental {
            label: "below beta, rate unavailable, or point-in-time predicate failed".into(),
        }
    };

    TierComputation {
        lane,
        tier: verdict.tier(),
        verdict,
        citation,
        rate: Some(rate),
        evidence_run: Some(evrun.run.clone()),
        window: window_ids,
        active_box: Some(active_box),
        pending_box: replay.pending_box.clone(),
        pending_boundary: replay.boundary_ordinal(),
        promotion_ordinal: replay.promotion_ordinal,
        accumulation: replay.accumulation,
        designation_history: replay.history.clone(),
        stale,
    }
}

// ---------------------------------------------------------------------------
// Result constructors.
// ---------------------------------------------------------------------------

fn invalid(lane: Lane, citation: Citation, mut checks: Vec<String>) -> TierComputation {
    checks.sort();
    checks.dedup();
    TierComputation {
        lane,
        tier: Tier::Experimental,
        verdict: TierVerdict::InvalidEvidence {
            failed_checks: checks,
        },
        citation,
        rate: None,
        evidence_run: None,
        window: Vec::new(),
        active_box: None,
        pending_box: None,
        pending_boundary: None,
        promotion_ordinal: None,
        accumulation: 0,
        designation_history: Vec::new(),
        stale: false,
    }
}

fn experimental(
    lane: Lane,
    citation: Citation,
    replay: &DesignationReplay,
    label: &str,
) -> TierComputation {
    TierComputation {
        lane,
        tier: Tier::Experimental,
        verdict: TierVerdict::Experimental {
            label: label.into(),
        },
        citation,
        rate: None,
        evidence_run: None,
        window: Vec::new(),
        active_box: replay.active_box.clone(),
        pending_box: replay.pending_box.clone(),
        pending_boundary: replay.boundary_ordinal(),
        promotion_ordinal: replay.promotion_ordinal,
        accumulation: replay.accumulation,
        designation_history: replay.history.clone(),
        stale: false,
    }
}

fn build_citation(corpus: &Corpus, release_commit: &str) -> Citation {
    use sha2::{Digest, Sha256};
    let journal_json = serde_json::to_vec(&corpus.journal).unwrap_or_default();
    let journal_content_digest =
        ArtifactDigest(format!("sha256:{:x}", Sha256::digest(&journal_json)));
    let journal_high_water = corpus
        .journal
        .entries()
        .iter()
        .map(|e| e.ordinal.get())
        .max()
        .unwrap_or(0);
    let (manifest_digest, aggregation_version) = corpus
        .commits
        .get(release_commit)
        .map(|c| {
            (
                Some(c.battery.manifest_digest()),
                Some(c.battery.aggregation_version.clone()),
            )
        })
        .unwrap_or((None, None));
    let (snapshot_ordinal, snapshot_attested_time) = corpus
        .journal
        .latest_snapshot()
        .map(|(o, s)| (Some(o.get()), Some(s.observed_at.clone())))
        .unwrap_or((None, None));
    Citation {
        release_commit: release_commit.to_string(),
        manifest_digest,
        aggregation_version,
        journal_content_digest,
        journal_high_water,
        index_head_digest: corpus.index.head_digest(),
        snapshot_ordinal,
        snapshot_attested_time,
    }
}

// ===========================================================================
// STAGE 1 validation checks (v0–v9). Each returns the failed-check strings it
// found (empty = pass); INVALID_EVIDENCE if any stage-1 check fails.
// ===========================================================================

/// v0 — id charset well-formedness (F-RT1b). The `"::"` sequence is the RESERVED
/// delimiter of a derived observation id (`ObservationId::derive` =
/// `"{run}::{lane}::{cell}"`) and of a grid cell key (`"{provider}::{cell}"`). If
/// a **run id** or a **cell id** may itself contain `"::"`, that structure is
/// ambiguous — a `"::"`-crafted run id `"X::Y"` makes its observation id
/// `"X::Y::lane::cell"` match the prefix `"X::"` of a DECOY `rec.run = "X"`,
/// re-opening the RT-1 attribution-laundering bypass past v9's `starts_with`
/// test. C-1's `RunId`/`CellId` are unconstrained `pub String` with no charset
/// guard, and the fresh-reader threat model does not trust corpus-supplied ids —
/// so this check removes the ambiguity at the source: any run id or cell id
/// containing `"::"`, anywhere in the corpus, is malformed → INVALID_EVIDENCE.
/// After this, `starts_with("{rec.run}::")` (v9) and `split_once("::")`
/// (`parse_cell_key`) are sound, and lane provider-ids (a fixed enum) are already
/// safe.
fn v0_id_wellformedness(corpus: &Corpus) -> Vec<String> {
    let mut errs = Vec::new();
    let mut bad_run = |ctx: &str, id: &str| {
        if id.contains("::") {
            errs.push(format!(
                "v0: run id {id:?} ({ctx}) contains the reserved delimiter \"::\" — ambiguous, malformed (F-RT1b)"
            ));
        }
    };
    // Run ids everywhere they appear: journal entries, artifact headers +
    // coverage records, and every attribution record's declared run.
    for e in corpus.journal.entries() {
        match &e.body {
            JournalBody::Run(r) => bad_run("journal run entry", &r.run.0),
            JournalBody::Start(s) => bad_run("run-start event", &s.run.0),
            JournalBody::Terminal(t) => bad_run("terminal event", &t.run.0),
            _ => {}
        }
    }
    for a in &corpus.index.artifacts {
        bad_run("artifact header", &a.artifact.header.tuple.run.0);
        for res in a.artifact.grid.values() {
            if let CellResolution::Coverage(cov) = res {
                bad_run("coverage record", &cov.run.0);
            }
        }
    }
    for rec in &corpus.attributions {
        bad_run("attribution record run", &rec.run.0);
    }
    // Cell ids: the manifest's cells and every observed / covered cell.
    let mut bad_cell = |ctx: &str, id: &str| {
        if id.contains("::") {
            errs.push(format!(
                "v0: cell id {id:?} ({ctx}) contains the reserved delimiter \"::\" — ambiguous, malformed (F-RT1b)"
            ));
        }
    };
    for ctx in corpus.commits.values() {
        for cell in &ctx.battery.cells {
            bad_cell("manifest cell", &cell.id.0);
        }
    }
    for a in &corpus.index.artifacts {
        for res in a.artifact.grid.values() {
            match res {
                CellResolution::Observed(o) => bad_cell("observation cell", &o.cell.0),
                CellResolution::Coverage(cov) => bad_cell("coverage cell", &cov.cell.0),
            }
        }
    }
    errs
}

/// v1 — journal integrity. The C-1 `integrity_check` covers ordinal/run-id/
/// token/nonce/terminal/start well-formedness. C-3 adds the corpus-scope
/// invariants A10 v1 names that C-1 could not: no forged promotion event kind
/// (R5-O5 — promotion is DERIVED, so any promotion event is structurally
/// invalid), and abort/void ruling authority + evidence validity.
fn v1_journal_integrity(journal: &AuthorityJournal) -> Vec<String> {
    let mut errs = Vec::new();
    if let Err(mut e) = journal.integrity_check() {
        errs.append(&mut e.iter_mut().map(|s| format!("v1: {s}")).collect());
    }
    for entry in journal.entries() {
        match &entry.body {
            JournalBody::Designation(DesignationEvent {
                kind: DesignationKind::Promotion,
                lane,
                ..
            }) => {
                errs.push(format!(
                    "v1: forged promotion event for lane {} — promotion is DERIVED replay arithmetic, no promotion event kind exists (R5-O5)",
                    lane.provider_id()
                ));
            }
            JournalBody::Terminal(t) => {
                if let TerminalState::Aborted(r) = &t.terminal {
                    if r.no_artifact_evidence.trim().is_empty() {
                        errs.push(format!(
                            "v1: abort ruling for run {:?} carries no outcome-independent evidence (R6-6)",
                            t.run.0
                        ));
                    }
                    if r.authorized_by.trim().is_empty() {
                        errs.push(format!(
                            "v1: abort ruling for run {:?} names no authorizing seat (R6-6)",
                            t.run.0
                        ));
                    } else if !seat_is_independent(journal, &t.run, &r.authorized_by) {
                        errs.push(format!(
                            "v1: abort ruling for run {:?} issued by the run's own executor/coordinator — not the independent void-and-abort seat (R6-6)",
                            t.run.0
                        ));
                    }
                }
                if let TerminalState::Voided(r) = &t.terminal {
                    if r.no_artifact_evidence.trim().is_empty() {
                        errs.push(format!(
                            "v1: void ruling for run {:?} carries no outcome-independent evidence (N-1)",
                            t.run.0
                        ));
                    }
                    if r.authorized_by.trim().is_empty() {
                        errs.push(format!(
                            "v1: void ruling for run {:?} names no authorizing seat (N-1)",
                            t.run.0
                        ));
                    } else if !seat_is_independent(journal, &t.run, &r.authorized_by) {
                        errs.push(format!(
                            "v1: void ruling for run {:?} issued by the run's own executor/coordinator — not independent (N-1)",
                            t.run.0
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    errs
}

/// Whether an authorizing seat identity is independent of the run's executor and
/// coordinator (the enforceable corpus-scope subset of the void-and-abort seat's
/// independence — lane-owner/cell-author exclusions need the role registry and
/// apply to attribution, A8).
fn seat_is_independent(journal: &AuthorityJournal, run: &RunId, seat: &str) -> bool {
    let executor = journal.start_event(run).map(|s| s.runner_identity.as_str());
    let coordinator = journal
        .run_entry(run)
        .map(|r| r.commissioning_identity.as_str());
    Some(seat) != executor && Some(seat) != coordinator
}

/// The digests cited by a completion event in the anchored prefix (R6-1) — the
/// consumable set. An index entry cited by NO completion event is corpus-invisible
/// (the benign write-then-append race / an unregistered run): never a candidate
/// and never a failure.
fn cited_digests(corpus: &Corpus) -> BTreeSet<String> {
    corpus
        .journal
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            JournalBody::Terminal(t) => match &t.terminal {
                TerminalState::Completed { artifact_digest } => Some(artifact_digest.0.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// v2 — the terminal-state cardinality matrix (R5-O4), validated over the
/// COMPLETE index at the snapshot-attested head with corpus visibility BY
/// CITATION (R6-1). Cardinality is per-run, keyed on terminal state; an uncited
/// artifact is corpus-invisible (benign).
fn v2_cardinality_matrix(corpus: &Corpus) -> Vec<String> {
    let mut errs = Vec::new();
    let journal = &corpus.journal;

    // Duplicate copies: two index entries filed under the same digest (R5-O4).
    let mut seen_digest: BTreeMap<&str, usize> = BTreeMap::new();
    for a in &corpus.index.artifacts {
        *seen_digest.entry(a.digest.0.as_str()).or_default() += 1;
    }
    for (d, n) in &seen_digest {
        if *n > 1 {
            errs.push(format!(
                "v2: duplicate copies — digest {d:?} filed {n} times in the index (R5-O4)"
            ));
        }
    }

    // All index entries grouped by the run id their header carries.
    let mut by_run: BTreeMap<&str, Vec<&IndexedArtifact>> = BTreeMap::new();
    for a in &corpus.index.artifacts {
        by_run
            .entry(a.artifact.header.tuple.run.0.as_str())
            .or_default()
            .push(a);
    }
    let cited = cited_digests(corpus);

    // Per run entry: cardinality by terminal state.
    for r in journal.run_entries() {
        let terminal = journal.terminal_state(&r.run);
        let arts = by_run.get(r.run.0.as_str());
        match &terminal {
            TerminalState::Completed { artifact_digest } => {
                // Completed => exactly one canonical artifact present at the cited
                // digest (a cited digest absent from the index fails).
                match corpus.index.get(artifact_digest) {
                    None => errs.push(format!(
                        "v2: run {:?} completed citing digest {:?}, absent from the index (R6-1)",
                        r.run.0, artifact_digest.0
                    )),
                    Some(art) => {
                        // Authentication: the artifact filed at the cited digest
                        // must have that digest as its own content digest.
                        if art.content_digest() != *artifact_digest {
                            errs.push(format!(
                                "v2: run {:?}'s cited artifact is mis-filed — content digest {:?} ≠ cited {:?} (R6-1)",
                                r.run.0,
                                art.content_digest().0,
                                artifact_digest.0
                            ));
                        }
                        if art.header.tuple.run != r.run {
                            errs.push(format!(
                                "v2: run {:?}'s completion cites an artifact whose header run is {:?} — not its canonical artifact",
                                r.run.0, art.header.tuple.run.0
                            ));
                        }
                    }
                }
                // Exactly one artifact for the run in the index — a second copy or
                // a conflicting same-ID artifact fails (R5-O4).
                if let Some(arts) = arts {
                    if arts.len() != 1 {
                        errs.push(format!(
                            "v2: completed run {:?} has {} artifacts in the index — completed ⇒ exactly one canonical artifact (duplicate/conflicting same-ID, R5-O4)",
                            r.run.0,
                            arts.len()
                        ));
                    } else if arts[0].digest != *artifact_digest {
                        errs.push(format!(
                            "v2: completed run {:?}'s sole index artifact {:?} is not the cited canonical digest {:?}",
                            r.run.0, arts[0].digest.0, artifact_digest.0
                        ));
                    }
                }
            }
            TerminalState::CancelledPreExecution { .. }
            | TerminalState::Voided(_)
            | TerminalState::Aborted(_) => {
                // Zero artifacts: an artifact for a cancelled/voided/aborted entry
                // fails — an abort/void can never shadow evidence (R6-6/N-1).
                if arts.map(|a| !a.is_empty()).unwrap_or(false) {
                    errs.push(format!(
                        "v2: run {:?} is {} but an artifact exists for it in the index — a non-producing terminal can never shadow evidence (R6-6/N-1)",
                        r.run.0,
                        terminal_name(&terminal)
                    ));
                }
            }
            TerminalState::NonTerminal => {
                // open => zero consumable. An uncited artifact for an open entry is
                // the benign write-then-append race (R6-1). A cited one would need
                // a completion event (terminal), impossible here — guard anyway.
                if let Some(arts) = arts {
                    for a in arts {
                        if cited.contains(a.digest.0.as_str()) {
                            errs.push(format!(
                                "v2: open run {:?} has a CITED artifact {:?} — an open entry has zero consumable artifacts (R6-1)",
                                r.run.0, a.digest.0
                            ));
                        }
                    }
                }
            }
        }
    }

    // A CITED artifact whose run has no commissioning entry is an orphan (fail);
    // an UNCITED one is corpus-invisible (the benign unregistered-run case).
    for a in &corpus.index.artifacts {
        if cited.contains(a.digest.0.as_str())
            && journal.run_entry(&a.artifact.header.tuple.run).is_none()
        {
            errs.push(format!(
                "v2: cited artifact {:?} is an orphan — its run {:?} has no commissioning entry (R6-1)",
                a.digest.0, a.artifact.header.tuple.run.0
            ));
        }
    }

    errs
}

/// The consumable artifacts (R6-1): those reached via a completion event's
/// citation. Only these are validated by v3/v5/v8 and consumed by the window — an
/// uncited artifact is corpus-invisible.
fn consumable_artifacts(corpus: &Corpus) -> Vec<(ArtifactDigest, &RunArtifact)> {
    let cited = cited_digests(corpus);
    corpus
        .index
        .artifacts
        .iter()
        .filter(|a| cited.contains(a.digest.0.as_str()))
        .map(|a| (a.digest.clone(), &a.artifact))
        .collect()
}

fn terminal_name(t: &TerminalState) -> &'static str {
    match t {
        TerminalState::Completed { .. } => "completed",
        TerminalState::CancelledPreExecution { .. } => "cancelled-pre-execution",
        TerminalState::Voided(_) => "voided",
        TerminalState::Aborted(_) => "aborted",
        TerminalState::NonTerminal => "non-terminal",
    }
}

/// v3 — issuance + tuple binding. Every consumable artifact's commissioning
/// header reproduces its journal entry's FULL immutable tuple exactly; orphan
/// artifacts (no journal entry) fail; a mismatch in ANY tuple field fails.
fn v3_issuance_and_tuple_binding(corpus: &Corpus) -> Vec<String> {
    let mut errs = Vec::new();
    for (digest, art) in consumable_artifacts(corpus) {
        let run = &art.header.tuple.run;
        match corpus.journal.run_entry(run) {
            None => errs.push(format!(
                "v3: orphan artifact {:?} — run {:?} has no journal entry",
                digest.0, run.0
            )),
            Some(entry) => {
                let t = &art.header.tuple;
                let mut mism = Vec::new();
                if t.lane_scope != entry.lane_scope {
                    mism.push("lane_scope");
                }
                if t.box_id != entry.box_id {
                    mism.push("box");
                }
                if t.release_commit != entry.release_commit {
                    mism.push("release_commit");
                }
                if t.manifest_digest != entry.manifest_digest {
                    mism.push("manifest_digest");
                }
                if t.aggregation_version != entry.aggregation_version {
                    mism.push("aggregation_version");
                }
                if t.run_kind != entry.run_kind {
                    mism.push("run_kind");
                }
                if t.sequence != entry.sequence {
                    mism.push("sequence");
                }
                if t.token != entry.token {
                    mism.push("token");
                }
                if !mism.is_empty() {
                    errs.push(format!(
                        "v3: artifact {:?} header tuple mismatches its journal entry in field(s): {} (N-1/R5-O3)",
                        run.0,
                        mism.join(", ")
                    ));
                }
            }
        }
    }
    errs
}

/// v5 — exact observation domain (R5-O3). Each consumable run's observation set
/// is EXACTLY its entry's planned domain (manifest-authorized (lane, cell) over
/// the commissioned scope): every applicable cell exactly one resolution,
/// declared NA/coverage only where the manifest permits, NO extra cells; an
/// observation for a lane outside the commissioned scope fails.
fn v5_exact_observation_domain(corpus: &Corpus, battery: &Battery) -> Vec<String> {
    let mut errs = Vec::new();
    for (_digest, art) in consumable_artifacts(corpus) {
        let entry = match corpus.journal.run_entry(&art.header.tuple.run) {
            Some(e) => e,
            None => continue, // orphan — v3 already flagged.
        };
        // Planned domain: the manifest-authorized (lane, cell) domain over the
        // commissioned lane scope. Derived from [lane scope, manifest digest] —
        // never a second free-floating source.
        let mut planned: BTreeSet<String> = BTreeSet::new();
        for lane in entry.lane_scope.lanes() {
            for cell in &battery.cells {
                if battery.applicability(lane, &cell.id).in_domain() {
                    planned.insert(RunArtifact::cell_key(lane, &cell.id));
                }
            }
        }
        // Every grid key must be in the planned domain, permitted, and unique;
        // every planned key must be present exactly once.
        for (key, resolution) in &art.grid {
            let (lane, cell) = match parse_cell_key(key) {
                Some(x) => x,
                None => {
                    errs.push(format!(
                        "v5: run {:?} has an unparseable grid key {key:?}",
                        art.header.tuple.run.0
                    ));
                    continue;
                }
            };
            if !entry.lane_scope.contains(lane) {
                errs.push(format!(
                    "v5: run {:?} carries an observation for lane {} outside its commissioned scope (R5-O3)",
                    art.header.tuple.run.0,
                    lane.provider_id()
                ));
                continue;
            }
            if !planned.contains(key) {
                errs.push(format!(
                    "v5: run {:?} carries an extra non-manifest observation {key:?} (N-3)",
                    art.header.tuple.run.0
                ));
            }
            if !battery.permits(lane, &cell, resolution) {
                errs.push(format!(
                    "v5: run {:?} resolves {key:?} in a way the manifest does not permit (N-3)",
                    art.header.tuple.run.0
                ));
            }
            // Identity binding (free-floating-field class, RT-5 audit): the inner
            // record's OWN identity fields must match the grid POSITION (the key's
            // lane/cell + the artifact's run). Otherwise a Fail can carry a decoy
            // `o.id` and be laundered by an attribution for that ghost id (the head
            // lookup keys on `o.id`, the pool on the key — a divergence). Binding
            // `o.id == derive(run, lane, cell)` forecloses it: an attribution can
            // only ever target a position-derived id, forcing v9's rec.run to the
            // real run and thus A8's executor exclusion.
            let run = &art.header.tuple.run;
            match resolution {
                CellResolution::Observed(o) => {
                    let expected = ObservationId::derive(run, lane, &cell);
                    if o.id != expected {
                        errs.push(format!(
                            "v5: run {:?} observation at {key:?} carries id {:?} ≠ its position-derived id {:?} — decoy observation id (RT-5 class)",
                            run.0, o.id.0, expected.0
                        ));
                    }
                    if o.lane != lane {
                        errs.push(format!(
                            "v5: run {:?} observation at {key:?} carries lane {} ≠ its grid-key lane {} (RT-5 class)",
                            run.0, o.lane.provider_id(), lane.provider_id()
                        ));
                    }
                    if o.cell != cell {
                        errs.push(format!(
                            "v5: run {:?} observation at {key:?} carries cell {:?} ≠ its grid-key cell {:?} (RT-5 class)",
                            run.0, o.cell.0, cell.0
                        ));
                    }
                    // Declared-gap honesty (QS-2), corpus-wide — NOT only on the
                    // evidence run (beta_pt). An undeclared not-run invalidates the
                    // grid; and a would-be FAIL relabelled BLOCKED with an empty
                    // reason on any WINDOW run would launder it out of the pool
                    // (Blocked is excluded from A3) — an inflation vector on a
                    // non-evidence window run that beta_pt (evrun-only) never saw.
                    match &o.outcome {
                        Outcome::Blocked { reason, reentry } => {
                            if reason.trim().is_empty() || reentry.trim().is_empty() {
                                errs.push(format!(
                                    "v5: run {:?} blocks {key:?} with an empty reason/re-entry — an undeclared not-run (QS-2)",
                                    run.0
                                ));
                            }
                        }
                        Outcome::NotApplicable { reason } => {
                            if reason.trim().is_empty() {
                                errs.push(format!(
                                    "v5: run {:?} marks {key:?} not-applicable with an empty reason — an undeclared gap (QS-2)",
                                    run.0
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                CellResolution::Coverage(cov) => {
                    // Coverage well-formedness (validated on WRITE in C-1's builder,
                    // but a forged corpus consumes it unvalidated) — re-checked here
                    // corpus-wide: an empty why-not-here / re-entry / authority is an
                    // undeclared gap (QS-2), parallel to the observation check above.
                    if let Err(e) = cov.well_formed() {
                        errs.push(format!(
                            "v5: run {:?} coverage record at {key:?} is malformed: {e} (QS-2)",
                            run.0
                        ));
                    }
                    if &cov.run != run {
                        errs.push(format!(
                            "v5: run {:?} coverage record at {key:?} names run {:?} ≠ its artifact's run (RT-5 class)",
                            run.0, cov.run.0
                        ));
                    }
                    if cov.lane != lane {
                        errs.push(format!(
                            "v5: run {:?} coverage record at {key:?} names lane {} ≠ its grid-key lane {} (RT-5 class)",
                            run.0, cov.lane.provider_id(), lane.provider_id()
                        ));
                    }
                    if cov.cell != cell {
                        errs.push(format!(
                            "v5: run {:?} coverage record at {key:?} names cell {:?} ≠ its grid-key cell {:?} (RT-5 class)",
                            run.0, cov.cell.0, cell.0
                        ));
                    }
                }
            }
        }
        for key in &planned {
            if !art.grid.contains_key(key) {
                errs.push(format!(
                    "v5: run {:?} is missing a resolution for planned-domain cell {key:?} (A2(d) fullness)",
                    art.header.tuple.run.0
                ));
            }
        }
    }
    errs
}

/// v6 — context existence/uniqueness/epoch-consistency + designation legality
/// (R5-O7/N-2/R6-2). Manifest + scheme-version exist at the release commit; every
/// run entry's claimed context equals the context effective at its ordinal; the
/// journal's LATEST effective context equals the release-derived context; and the
/// lane's designation events replay to a legal A9 state.
fn v6_context_epoch_and_designation(
    corpus: &Corpus,
    _lane: Lane,
    manifest_digest: &ManifestDigest,
    agg_version: &AggregationVersion,
    replay: &DesignationReplay,
) -> Vec<String> {
    let mut errs = Vec::new();
    let journal = &corpus.journal;

    // Each run entry's claimed context equals the context effective at its
    // ordinal; a run entry preceding any context activation fails (uncovered).
    for r in journal.run_entries() {
        let ord = JournalOrdinal::new(r.sequence.get());
        match journal.manifest_digest_effective_at(ord) {
            None => errs.push(format!(
                "v6: run {:?} at ordinal {} precedes any manifest context-activation (R5-O7)",
                r.run.0,
                r.sequence.get()
            )),
            Some(eff) if eff != &r.manifest_digest => errs.push(format!(
                "v6: run {:?} claims manifest {:?} but {:?} is effective at its ordinal (R5-O7)",
                r.run.0, r.manifest_digest.0, eff.0
            )),
            _ => {}
        }
        match journal.aggregation_version_effective_at(ord) {
            None => errs.push(format!(
                "v6: run {:?} at ordinal {} precedes any aggregation-version context-activation (R5-O7)",
                r.run.0,
                r.sequence.get()
            )),
            Some(eff) if eff != &r.aggregation_version => errs.push(format!(
                "v6: run {:?} claims aggregation {:?} but {:?} is effective at its ordinal (R5-O7)",
                r.run.0, r.aggregation_version.0, eff.0
            )),
            _ => {}
        }
    }

    // The journal's LATEST effective context must equal the release-derived
    // context — two candidate contexts are never a reader's pick (R5-O7).
    let hw = JournalOrdinal::new(
        journal
            .entries()
            .iter()
            .map(|e| e.ordinal.get())
            .max()
            .unwrap_or(0),
    );
    match journal.manifest_digest_effective_at(hw) {
        None => errs.push(
            "v6: journal has no manifest context-activation — not yet activated for this release (R5-O7)"
                .into(),
        ),
        Some(eff) if eff != manifest_digest => errs.push(format!(
            "v6: journal's latest effective manifest {:?} disagrees with the release-derived {:?} (R5-O7)",
            eff.0, manifest_digest.0
        )),
        _ => {}
    }
    match journal.aggregation_version_effective_at(hw) {
        None => errs.push(
            "v6: journal has no aggregation-version context-activation for this release (R5-O7)".into(),
        ),
        Some(eff) if eff != agg_version => errs.push(format!(
            "v6: journal's latest effective aggregation {:?} disagrees with the release-derived {:?} (R5-O7)",
            eff.0, agg_version.0
        )),
        _ => {}
    }

    // Designation legality (A9): exactly one active, at most one pending,
    // withdrawals only on enumerated outcome-independent bases, only before the
    // derived promotion ordinal, AND only in a quiet window (R6-2).
    errs.extend(replay.legality_errors.iter().map(|e| format!("v6: {e}")));

    errs
}

/// v7 — publication ANCHORING coherence (R6-5), the per-corpus half. The corpus
/// is anchored at exactly one publication-snapshot event: the journal prefix's
/// high-water IS the snapshot's ordinal, and the index head equals the snapshot's
/// attested digest. A corpus whose heads are not bound by a snapshot fails.
///
/// **The other two v7 obligations — a CURRENT publication must anchor at the
/// registrar's LATEST snapshot, and NO-ROLLBACK (a published row's snapshot
/// ordinal ≥ every previously-published row's for the table, checkable from T4's
/// append-only history) — are NOT per-corpus checks:** they compare the corpus
/// against the registrar's latest snapshot and against prior published rows, both
/// of which live OUTSIDE `tier()`'s (corpus, release_commit) signature. They live
/// in the T4 publication layer — see [`super::tier_doc::publish_tier_document`]
/// and [`super::tier_doc::PublicationHistory`] — because the exploit they close (a
/// truncated-prefix corpus re-anchored at an earlier snapshot and published as
/// "current" to roll back to a more favourable window) is invisible to the
/// per-corpus check: the truncated prefix's own high-water IS that earlier
/// snapshot, so it passes anchoring coherence here.
fn v7_publication_anchoring(corpus: &Corpus) -> Vec<String> {
    let mut errs = Vec::new();
    let journal = &corpus.journal;
    match journal.latest_snapshot() {
        None => {
            errs.push("v7: corpus is not anchored at any publication-snapshot event (R6-5)".into())
        }
        Some((ord, snap)) => {
            let hw = journal
                .entries()
                .iter()
                .map(|e| e.ordinal.get())
                .max()
                .unwrap_or(0);
            if ord.get() != hw {
                errs.push(format!(
                    "v7: anchoring snapshot ordinal {} is not the journal high-water {hw} — the prefix must end at the snapshot (R6-5)",
                    ord.get()
                ));
            }
            let head = corpus.index.head_digest();
            if snap.artifact_index_head_digest != head {
                errs.push(format!(
                    "v7: index head digest {:?} does not equal the snapshot's attested digest {:?} (R6-5)",
                    head.0, snap.artifact_index_head_digest.0
                ));
            }
        }
    }
    errs
}

/// v8 — launch-nonce binding (R7-2). Every consumable completed artifact
/// reproduces, in its header, the launch nonce its entry's run-start event
/// discloses, and every green cell's proof-of-run carries that nonce's binding.
fn v8_launch_nonce_binding(corpus: &Corpus) -> Vec<String> {
    let mut errs = Vec::new();
    for (digest, art) in consumable_artifacts(corpus) {
        let run = &art.header.tuple.run;
        let start_nonce = corpus.journal.start_event(run).map(|s| &s.launch_nonce);
        match start_nonce {
            None => {
                // No run-start event. A consumable completed artifact needs one;
                // v1/v2 catch the terminal side. Here: an artifact whose header
                // carries a nonce with no run-start event to have disclosed it.
                errs.push(format!(
                    "v8: artifact {:?} carries a launch nonce but its run {:?} has no run-start event to disclose one (R7-2)",
                    digest.0, run.0
                ));
            }
            Some(n) => {
                if &art.header.launch_nonce != n {
                    errs.push(format!(
                        "v8: artifact {:?} header nonce differs from its run-start event's disclosed nonce (R7-2)",
                        run.0
                    ));
                }
                for obs in art.observations() {
                    if let Some(proof) = obs.outcome.proof() {
                        if proof.launch_nonce() != &art.header.launch_nonce {
                            errs.push(format!(
                                "v8: green proof-of-run for {:?} carries a nonce not bound to the artifact header (R7-2)",
                                obs.id.0
                            ));
                        }
                    }
                }
            }
        }
    }
    errs
}

/// v9 — attribution record run-binding (A8/F-2, RT-1). Every attribution record
/// carries a single `run` field, and it governs its `observations` — whose ids
/// embed their OWN run (`ObservationId::derive` = `"{run}::{lane}::{cell}"`).
/// A8's authority exclusion (issuer ≠ the run's executor/coordinator) is keyed on
/// `rec.run`; NOTHING otherwise binds `rec.run` to the observations' run, so a
/// record could declare a decoy `rec.run` (absent from the journal → both
/// exclusions skipped) while ruling on a REAL run's observation — laundering a
/// fail issued by that run's own executor. This check forecloses it structurally:
/// every governed observation id MUST begin with `"{rec.run}::"`; any mismatch is
/// a malformed record → INVALID_EVIDENCE (loud, per C-3's totality contract). A
/// record is single-run by construction (attribution.rs — one `run` field); a
/// multi-run record is not the model. This makes the `rec.run`-keyed exclusion in
/// [`authority_valid`] sound (rec.run == every governed observation's run).
fn v9_attribution_run_binding(corpus: &Corpus) -> Vec<String> {
    let mut errs = Vec::new();
    for rec in &corpus.attributions {
        // Local guard (F-RT1b, part b): a `rec.run` containing the reserved
        // delimiter `"::"` makes the `starts_with` test below unsound (a decoy
        // prefix segment). v0 already rejects any such run id corpus-wide; this
        // keeps v9 sound on its own, so it never launders even if v0 were changed.
        if rec.run.0.contains("::") {
            errs.push(format!(
                "v9: attribution record {:?} declares a run {:?} containing the reserved delimiter \"::\" — ambiguous, malformed (F-RT1b)",
                rec.id.0, rec.run.0
            ));
            continue;
        }
        let prefix = format!("{}::", rec.run.0);
        for obs in &rec.observations {
            if !obs.0.starts_with(&prefix) {
                errs.push(format!(
                    "v9: attribution record {:?} declares run {:?} but governs observation {:?} whose run differs — a record is single-run; a decoy run cannot launder another run's fail (A8/RT-1)",
                    rec.id.0, rec.run.0, obs.0
                ));
            }
        }
    }
    errs
}

// ===========================================================================
// STAGE 2 — designation-epoch replay (A9/N-2/R5-O5/R6-2). Promotion is DERIVED at
// the terminal-completion ordinal of the Wth eligible pending-box run.
// ===========================================================================

/// The derived designation state for a lane.
#[derive(Debug, Clone, Default)]
pub struct DesignationReplay {
    pub active_box: Option<String>,
    /// The ACTIVE box's effective-since ordinal (A9 pre-outcome standing): only
    /// runs whose commissioning sequence is greater than this count toward the
    /// active window (the stage-3 `e.sequence > boundary` gate). It is the ordinal
    /// of the designation event that made the current box active — the initial
    /// Designate's ordinal, or (after a promotion) the PendingReplacement event's
    /// effective-after ordinal.
    pub active_boundary: u64,
    pub pending_box: Option<String>,
    /// The pending's effective-after boundary (its own journal ordinal).
    pub boundary: Option<u64>,
    pub promotion_ordinal: Option<u64>,
    /// The full append-only designation history for the lane (T4 publishes it).
    pub history: Vec<DesignationHistoryEntry>,
    /// Accumulation state toward promotion: the count of eligible pending-box
    /// runs seen (0..W), for T4's honest surfacing.
    pub accumulation: u64,
    pub legality_errors: Vec<String>,
}

/// One entry in a lane's published designation history (T4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignationHistoryEntry {
    pub ordinal: u64,
    pub kind: String,
    pub box_id: String,
    pub policy_basis: String,
    pub rationale: String,
    pub authorized_by: String,
    pub timestamp: String,
}

impl DesignationReplay {
    fn boundary_ordinal(&self) -> Option<u64> {
        self.boundary
    }
}

// Basis admissibility (RT-6): the basis is a TYPED closed enum
// [`DesignationBasisKind`], NOT a free-text word-denylist (a denylist is
// synonym/spacing/homoglyph-incomplete by construction). Outcome-independence is
// structural — every kind is an outcome-independent criterion, and there is no
// free text for an outcome to be smuggled into. An ABSENT kind (`None`) is
// INVALID_EVIDENCE, never a permissive default. Admissibility per arm differs
// (withdrawal is stricter than re-designation, which is stricter than the initial
// designation) — the spec grounds each below.

/// The kinds a WITHDRAWAL may cite (R5-O5): the pending box decommissioned /
/// durably unreachable before promotion, or its runnability basis invalidated.
/// NOT `GreaterRunnability` — you do not withdraw a pending because some box has
/// greater runnability.
fn withdrawal_kind_ok(kind: Option<DesignationBasisKind>) -> bool {
    matches!(
        kind,
        Some(
            DesignationBasisKind::Decommissioned
                | DesignationBasisKind::DurablyUnreachable
                | DesignationBasisKind::RunnabilityReduced
        )
    )
}

/// The kinds a (pending) RE-DESIGNATION may cite (A9): the current box
/// decommissioned / durably unreachable / runnability materially reduced (reasons
/// to leave), or a new box with strictly greater runnability (reason to go) — all
/// four.
fn redesignation_kind_ok(kind: Option<DesignationBasisKind>) -> bool {
    matches!(
        kind,
        Some(
            DesignationBasisKind::Decommissioned
                | DesignationBasisKind::DurablyUnreachable
                | DesignationBasisKind::RunnabilityReduced
                | DesignationBasisKind::GreaterRunnability
        )
    )
}

/// The kind an INITIAL designation may cite (A9): the box selected for maximal /
/// strictly greater applicable-cell runnability at representative shape. The
/// leave-reasons (decommissioned/unreachable/reduced) do not apply to a first
/// designation.
fn initial_designation_kind_ok(kind: Option<DesignationBasisKind>) -> bool {
    matches!(kind, Some(DesignationBasisKind::GreaterRunnability))
}

/// Replay the lane's designation events (A9). Derives the active box, any pending
/// replacement + its boundary, and the promotion ordinal (replay arithmetic).
/// Records legality errors (checked by v6). The manifest/agg args are the
/// release-derived context — accumulation eligibility judges each run under the
/// context effective at its own commissioning ordinal (R5-O7), so they are read
/// from the journal, not these; these are threaded for the eligibility helper's
/// current-context comparison at the boundary.
fn replay_designations(
    journal: &AuthorityJournal,
    lane: Lane,
    _manifest_digest: &ManifestDigest,
    _agg_version: &AggregationVersion,
    params: &TierParams,
) -> DesignationReplay {
    let mut r = DesignationReplay::default();

    // Gather the lane's designation events in ordinal order.
    let mut events: Vec<(u64, &DesignationEvent)> = journal
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            JournalBody::Designation(d) if d.lane == lane => Some((e.ordinal.get(), d)),
            _ => None,
        })
        .collect();
    events.sort_by_key(|(o, _)| *o);

    for (ord, ev) in &events {
        r.history.push(DesignationHistoryEntry {
            ordinal: *ord,
            kind: format!("{:?}", ev.kind),
            box_id: ev.box_id.0.clone(),
            policy_basis: ev.policy_basis.clone(),
            rationale: ev.rationale.clone(),
            authorized_by: ev.authorized_by.clone(),
            timestamp: ev.timestamp.clone(),
        });
        match ev.kind {
            DesignationKind::Promotion => {
                // No promotion event kind exists (R5-O5) — v1 flags it; note here
                // too so replay legality is self-contained.
                r.legality_errors.push(format!(
                    "designation replay: a promotion event at ordinal {ord} is structurally invalid — promotion is derived (R5-O5)"
                ));
            }
            DesignationKind::Designate => {
                if r.active_box.is_some() {
                    r.legality_errors.push(format!(
                        "designation replay: a second initial Designate at ordinal {ord} while an active designation stands — re-designation must be a PendingReplacement (A9)"
                    ));
                } else {
                    // A9: the designation is selected by a preregistered
                    // OUTCOME-INDEPENDENT criterion — the typed basis kind
                    // (absent/inadmissible → INVALID).
                    if !initial_designation_kind_ok(ev.basis_kind) {
                        r.legality_errors.push(format!(
                            "designation replay: an initial Designate at ordinal {ord} carries basis kind {:?}, not an admissible outcome-independent designation kind (A9/RT-6)",
                            ev.basis_kind
                        ));
                    }
                    r.active_box = Some(ev.box_id.0.clone());
                    // The initial designation's ordinal is the active box's floor
                    // (A9 pre-outcome standing): only runs after it count.
                    r.active_boundary = *ord;
                }
            }
            DesignationKind::PendingReplacement => {
                if r.active_box.is_none() {
                    r.legality_errors.push(format!(
                        "designation replay: a PendingReplacement at ordinal {ord} with no active designation to replace (A9)"
                    ));
                } else if r.pending_box.is_some() {
                    r.legality_errors.push(format!(
                        "designation replay: a second PendingReplacement at ordinal {ord} while one is outstanding — the outstanding one must be withdrawn first (A9)"
                    ));
                } else {
                    // A9 re-designation: permitted ONLY on an admissible typed
                    // outcome-independent kind — closing the gaming vector
                    // (re-designate the evidence box on observed outcomes). The
                    // typed kind makes outcome-independence structural (RT-6).
                    if !redesignation_kind_ok(ev.basis_kind) {
                        r.legality_errors.push(format!(
                            "designation replay: a PendingReplacement at ordinal {ord} carries basis kind {:?}, not an admissible outcome-independent re-designation kind (A9/RT-6)",
                            ev.basis_kind
                        ));
                    }
                    r.pending_box = Some(ev.box_id.0.clone());
                    r.boundary = Some(*ord);
                    // Derive promotion, if the Wth eligible pending-box run has
                    // already completed (replay arithmetic — R5-O5). On promotion
                    // the pending box becomes active with THIS event's ordinal as
                    // its effective-since floor.
                    derive_promotion(journal, lane, &mut r, *ord, ev.box_id.0.clone(), params);
                }
            }
            DesignationKind::Withdrawal => {
                // A withdrawal at or after the derived promotion ordinal is invalid
                // — you cannot withdraw what has already promoted (R5-O5). Checked
                // FIRST, because a promotion clears `pending_box`, so a post-
                // promotion withdrawal must be diagnosed as post-promotion, not as
                // "no outstanding pending".
                if let Some(promo) = r.promotion_ordinal {
                    if *ord >= promo {
                        r.legality_errors.push(format!(
                            "designation replay: a Withdrawal at ordinal {ord} at or after the derived promotion ordinal {promo} — cannot withdraw what promoted (R5-O5)"
                        ));
                    }
                }
                if r.pending_box.is_none() && r.promotion_ordinal.map(|p| *ord < p).unwrap_or(true)
                {
                    r.legality_errors.push(format!(
                        "designation replay: a Withdrawal at ordinal {ord} with no outstanding pending replacement (A9)"
                    ));
                } else if r.pending_box.is_some() {
                    let pending = r.pending_box.clone().unwrap();
                    // RT-5: a withdrawal must NAME the pending box it retires. A
                    // valid-but-mis-targeted withdrawal (correct basis, quiet, but
                    // `box_id` ≠ the outstanding pending) must NOT retire the pending
                    // — that would suppress a legitimate promotion / keep a favorable
                    // incumbent while T4 history shows a withdrawal of an unrelated
                    // box. `box_id` unbound to the pending it retires is malformed →
                    // INVALID_EVIDENCE.
                    if ev.box_id.0 != pending {
                        r.legality_errors.push(format!(
                            "designation replay: a Withdrawal at ordinal {ord} names box {:?} but the outstanding pending is {:?} — a withdrawal must name the pending box it retires (RT-5)",
                            ev.box_id.0, pending
                        ));
                    } else {
                        // Outcome-independent typed kind only (R5-O5/RT-6).
                        if !withdrawal_kind_ok(ev.basis_kind) {
                            r.legality_errors.push(format!(
                                "designation replay: a Withdrawal at ordinal {ord} carries basis kind {:?}, not an admissible outcome-independent withdrawal kind (R5-O5/RT-6)",
                                ev.basis_kind
                            ));
                        }
                        // Quiet window (R6-2): at the withdrawal's ordinal, no
                        // started-not-terminal evidence-kind run may exist on the
                        // pending (lane, box).
                        if !quiet_window(journal, lane, &pending, *ord) {
                            r.legality_errors.push(format!(
                                "designation replay: a Withdrawal at ordinal {ord} lands during a started-not-terminal pending-box evidence run — not a quiet window (R6-2)"
                            ));
                        }
                        // Only retire if it did not already promote (promotion wins).
                        if r.promotion_ordinal.map(|p| *ord < p).unwrap_or(true) {
                            r.pending_box = None;
                            r.boundary = None;
                            r.accumulation = 0;
                        }
                    }
                }
            }
        }
    }
    r
}

/// Derive the promotion ordinal for a pending replacement (R5-O5). The pending
/// promotes automatically at the terminal-completion ordinal of the Wth eligible
/// pending-box run. Accumulation eligibility = ALL of A2(a)-(e), the pending box
/// + boundary substituted in (a), lane-in-scope and run_kind==evidence retained,
/// judged under the context effective at each run's own commissioning ordinal
/// (R5-O5/O7); a context change mid-accumulation resets the pending window.
fn derive_promotion(
    journal: &AuthorityJournal,
    lane: Lane,
    r: &mut DesignationReplay,
    boundary: u64,
    pending_box: String,
    params: &TierParams,
) {
    // The pending window resets on any context-activation after the boundary —
    // so the effective boundary is max(boundary, last context activation ordinal).
    let last_ctx_change = journal
        .entries()
        .iter()
        .filter(|e| matches!(e.body, JournalBody::ContextActivation(_)))
        .map(|e| e.ordinal.get())
        .filter(|o| *o > boundary)
        .max();
    let eff_boundary = last_ctx_change.unwrap_or(boundary);

    // Collect eligible pending-box completed runs with sequence > eff_boundary,
    // each judged under the context effective at its own commissioning ordinal.
    let mut eligible: Vec<(u64, u64, String)> = Vec::new(); // (sequence, terminal_ord, run_id)
    for entry in journal.run_entries() {
        if !entry.lane_scope.contains(lane) {
            continue;
        }
        if entry.box_id.0 != pending_box {
            continue;
        }
        if entry.run_kind != RunKind::Evidence {
            continue;
        }
        if entry.sequence.get() <= eff_boundary {
            continue;
        }
        // Terminal must be completed.
        let terminal_ord = match terminal_ordinal(journal, &entry.run) {
            Some((ord, TerminalState::Completed { .. })) => ord,
            _ => continue,
        };
        // Claimed == effective context at its ordinal (v6 guarantees for a valid
        // corpus; re-checked here so promotion is self-consistent).
        let ord = JournalOrdinal::new(entry.sequence.get());
        let m_ok = journal
            .manifest_digest_effective_at(ord)
            .map(|d| d == &entry.manifest_digest)
            .unwrap_or(false);
        let a_ok = journal
            .aggregation_version_effective_at(ord)
            .map(|v| v == &entry.aggregation_version)
            .unwrap_or(false);
        if !m_ok || !a_ok {
            continue;
        }
        eligible.push((entry.sequence.get(), terminal_ord, entry.run.0.clone()));
    }
    // Order by (sequence, run_id) — F-3.
    eligible.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
    r.accumulation = eligible.len() as u64;
    if eligible.len() >= params.window {
        // The Wth eligible run's TERMINAL ordinal is the (candidate) promotion
        // ordinal.
        let wth = &eligible[params.window - 1];
        let p = wth.1;
        // A9 over-strictness fix (RT-ASSESS): a valid quiet withdrawal issued at
        // an ordinal BEFORE the candidate promotion ordinal RETIRES the pending
        // first — the promotion never derives ("a withdrawal appended before the
        // derived promotion ordinal is valid", R5-O5). Only promote if NO
        // withdrawal for this pending lands in `(boundary, p]`. If one does, leave
        // the pending outstanding so the Withdrawal handler retires it (subject to
        // its own quiet-window + outcome-independent-basis checks). A withdrawal
        // AFTER `p` is the post-promotion case the Withdrawal handler rejects.
        let preempting_withdrawal = journal.entries().iter().any(|e| {
            matches!(&e.body, JournalBody::Designation(d)
                if d.lane == lane
                    && d.kind == DesignationKind::Withdrawal
                    && d.box_id.0 == pending_box   // RT-5: only a withdrawal NAMING
                                                   // this pending box preempts it
                    && e.ordinal.get() > boundary
                    && e.ordinal.get() <= p)
        });
        if !preempting_withdrawal {
            r.promotion_ordinal = Some(p);
            r.active_box = Some(pending_box);
            // The promoted box's active floor is the pending event's
            // effective-after boundary — only runs after it count toward the
            // now-active window (A9).
            r.active_boundary = boundary;
            r.pending_box = None;
            r.boundary = None;
        }
    }
}

/// The terminal transition ordinal + state for a run, if terminal.
fn terminal_ordinal(journal: &AuthorityJournal, run: &RunId) -> Option<(u64, TerminalState)> {
    journal.entries().iter().find_map(|e| match &e.body {
        JournalBody::Terminal(t) if &t.run == run => Some((e.ordinal.get(), t.terminal.clone())),
        _ => None,
    })
}

/// A quiet window at ordinal `at` for pending (lane, box): no evidence-kind run
/// with the lane in scope and the pending box has a run-start event at an earlier
/// ordinal without a terminal event at an earlier ordinal (R6-2).
fn quiet_window(journal: &AuthorityJournal, lane: Lane, pending_box: &str, at: u64) -> bool {
    for entry in journal.run_entries() {
        if !entry.lane_scope.contains(lane) || entry.box_id.0 != pending_box {
            continue;
        }
        if entry.run_kind != RunKind::Evidence {
            continue;
        }
        let start_ord = journal.start_ordinal(&entry.run);
        if let Some(so) = start_ord {
            if so < at {
                let term_before = terminal_ordinal(journal, &entry.run)
                    .map(|(o, _)| o < at)
                    .unwrap_or(false);
                if !term_before {
                    return false; // started-not-terminal at `at`.
                }
            }
        }
    }
    true
}

// ===========================================================================
// STAGE 3 — eligible candidates (A2 named pre-eligibility, journal-side facts).
// ===========================================================================

/// A candidate = an eligible completed evidence run + its consumable artifact.
struct Candidate {
    run: RunId,
    sequence: RunSequence,
    artifact: RunArtifact,
}

fn eligible_candidates(
    corpus: &Corpus,
    lane: Lane,
    active_box: &str,
    manifest_digest: &ManifestDigest,
    agg_version: &AggregationVersion,
    active_boundary: u64,
    battery: &Battery,
) -> Vec<Candidate> {
    let journal = &corpus.journal;
    let mut out = Vec::new();
    for entry in journal.run_entries() {
        // A2(a): commissioned for the lane, on the ACTIVE designated box.
        if !entry.lane_scope.contains(lane) {
            continue;
        }
        if entry.box_id.0 != active_box {
            continue;
        }
        // A2(a): declared run kind is evidence (exercise never enters a window).
        if entry.run_kind != RunKind::Evidence {
            continue;
        }
        // completed entries ONLY (candidates drawn from completed — stage 3).
        let cited_digest = match journal.terminal_state(&entry.run) {
            TerminalState::Completed { artifact_digest } => artifact_digest,
            _ => continue,
        };
        // A2(b)/(c): current manifest digest and aggregation version (authority
        // side of the tuple).
        if &entry.manifest_digest != manifest_digest {
            continue;
        }
        if &entry.aggregation_version != agg_version {
            continue;
        }
        // A9: pre-outcome standing — sequence strictly greater than the active
        // box's effective-since boundary (the initial Designate's ordinal, or the
        // pending event's ordinal after a promotion). A run commissioned before
        // the box was designated never counts toward its window (stage-3
        // `e.sequence > boundary`).
        if entry.sequence.get() <= active_boundary {
            continue;
        }
        // A2(d): full for the lane. A2(e): header reproduces the tuple.
        let artifact = match corpus.index.get(&cited_digest) {
            Some(a) => a.clone(),
            None => continue, // v2 flags an absent cited digest.
        };
        if !full_for_lane(&artifact, lane, battery) {
            continue;
        }
        if !header_matches(&artifact, entry) {
            continue;
        }
        out.push(Candidate {
            run: entry.run.clone(),
            sequence: entry.sequence,
            artifact,
        });
    }
    out
}

/// A2(d) per-lane fullness: every manifest-applicable (lane, cell) resolved to
/// exactly one resolution in the grid (fullness for lane L reads only L's cells).
fn full_for_lane(artifact: &RunArtifact, lane: Lane, battery: &Battery) -> bool {
    for cell in &battery.cells {
        if battery.applicability(lane, &cell.id).in_domain() {
            let key = RunArtifact::cell_key(lane, &cell.id);
            if !artifact.grid.contains_key(&key) {
                return false;
            }
        }
    }
    true
}

/// A2(e): the artifact's commissioning header reproduces the entry's full tuple.
fn header_matches(artifact: &RunArtifact, entry: &super::journal::RunEntry) -> bool {
    let t = &artifact.header.tuple;
    t.run == entry.run
        && t.lane_scope == entry.lane_scope
        && t.box_id == entry.box_id
        && t.release_commit == entry.release_commit
        && t.manifest_digest == entry.manifest_digest
        && t.aggregation_version == entry.aggregation_version
        && t.run_kind == entry.run_kind
        && t.sequence == entry.sequence
        && t.token == entry.token
}

// ===========================================================================
// STAGE 4 — terminal completeness through the cutoff (fail-closed).
// ===========================================================================

/// v4 — every journal run-entry with lane in commissioned scope, box == box,
/// run_kind == evidence, and sequence <= candidate_cutoff is TERMINAL. A
/// non-terminal entry at or below the cutoff => INVALID_EVIDENCE (a registered
/// attempt below the would-be evidence run can never be waited out or silently
/// skipped).
fn v4_terminal_complete(
    corpus: &Corpus,
    lane: Lane,
    active_box: &str,
    candidate_cutoff: u64,
) -> Vec<String> {
    let mut errs = Vec::new();
    for entry in corpus.journal.run_entries() {
        if !entry.lane_scope.contains(lane) || entry.box_id.0 != active_box {
            continue;
        }
        if entry.run_kind != RunKind::Evidence {
            continue;
        }
        if entry.sequence.get() > candidate_cutoff {
            continue; // in-flight above the cutoff — stage 5 governs liveness.
        }
        if !corpus.journal.terminal_state(&entry.run).is_terminal() {
            errs.push(format!(
                "v4: run {:?} at sequence {} (≤ candidate cutoff {}) is non-terminal — a registered attempt below the evidence run cannot be skipped (R5-O2/N-1)",
                entry.run.0,
                entry.sequence.get(),
                candidate_cutoff
            ));
        }
    }
    errs
}

// ===========================================================================
// STAGE 5 — run-liveness suppression + per-observation classification (A5/A6/A8).
// ===========================================================================

/// Started-open evidence entries for (lane, box) whose run-start attested time is
/// more than delta older than the anchoring snapshot's attested time (R6-1).
fn stale_in_flight(
    corpus: &Corpus,
    lane: Lane,
    active_box: &str,
    now: &str,
    delta_secs: i64,
) -> Vec<RunId> {
    // An unparseable "now" (the anchoring snapshot's attested time) fails CLOSED,
    // symmetric with the start-time side: the corpus cannot prove any started-open
    // entry is fresh, so it must not protect a tier. (A malformed-but-anchored
    // snapshot timestamp must never defeat Δ suppression — the fail-open asymmetry
    // it would otherwise create is a real hole, even under the trust model.)
    let now_epoch = parse_rfc3339(now);
    let mut out = Vec::new();
    for entry in corpus.journal.run_entries() {
        if !entry.lane_scope.contains(lane) || entry.box_id.0 != active_box {
            continue;
        }
        if entry.run_kind != RunKind::Evidence {
            continue;
        }
        let started = corpus.journal.has_start_event(&entry.run);
        let terminal = corpus.journal.terminal_state(&entry.run).is_terminal();
        if started && !terminal {
            // Read the run-start event's attested time (R6-1). A started-open
            // entry more than delta older than the anchoring snapshot suppresses
            // the lane's window-dependent claims — a withheld completion kills the
            // tier it was protecting. An absent/unparseable start time OR an
            // unparseable "now" fails closed (suppresses): freshness must be
            // PROVEN, never assumed.
            let start_epoch = corpus
                .journal
                .start_event(&entry.run)
                .map(|s| s.started_at.clone())
                .filter(|t| !t.trim().is_empty())
                .and_then(|t| parse_rfc3339(&t));
            match (now_epoch, start_epoch) {
                (Some(n), Some(st)) => {
                    if n - st > delta_secs {
                        out.push(entry.run.clone());
                    }
                }
                // Either side unparseable → cannot prove freshness → suppress.
                _ => out.push(entry.run.clone()),
            }
        }
    }
    out
}

/// The per-observation classification (A1/A3/A6/A8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObsClass {
    Pass,
    FailProduct,
    FailPending,
    FailHarnessRemoved,
    Blocked,
    Na,
    Coverage,
}

/// Resolve the unique active head per observation from the authority-VALID
/// records only (A8). An authority-invalid record (issuer is the run's executor/
/// coordinator/lane-owner/cell-author) is dropped before head resolution, so it
/// never governs and leaves any existing valid head in force (A6).
fn resolve_valid_heads(corpus: &Corpus, lane: Lane) -> BTreeMap<ObservationId, Disposition> {
    // Map observation id → lane, via the index (an observation id encodes its
    // run; we need the lane to apply lane-scoped authority exclusions).
    let valid: Vec<AttributionRecord> = corpus
        .attributions
        .iter()
        .filter(|rec| authority_valid(corpus, rec, lane))
        .cloned()
        .collect();
    let resolution = resolve_heads(&valid);
    // head record id → disposition.
    let disp_of: BTreeMap<&str, Disposition> = valid
        .iter()
        .map(|r| (r.id.0.as_str(), r.disposition))
        .collect();
    let mut out = BTreeMap::new();
    for (obs, head_id) in &resolution.heads {
        if let Some(d) = disp_of.get(head_id.0.as_str()) {
            out.insert(obs.clone(), *d);
        }
    }
    out
}

/// A8 authority: the attribution seat MUST NOT be the run's executor, the run's
/// coordinator, the affected lane's owner, or an author of any cell in the
/// affected lane. Executor/coordinator are journal-derived; owner/author come
/// from the role registry.
fn authority_valid(corpus: &Corpus, rec: &AttributionRecord, lane: Lane) -> bool {
    let issuer = rec.issuer.trim();
    if issuer.is_empty() {
        return false;
    }
    // The run's executor + coordinator.
    if let Some(start) = corpus.journal.start_event(&rec.run) {
        if start.runner_identity == issuer {
            return false;
        }
    }
    if let Some(entry) = corpus.journal.run_entry(&rec.run) {
        if entry.commissioning_identity == issuer {
            return false;
        }
    }
    // The affected lane's owner + cell authors.
    if corpus
        .roles
        .lane_owners
        .get(&lane)
        .map(|o| o == issuer)
        .unwrap_or(false)
    {
        return false;
    }
    if corpus
        .roles
        .cell_authors
        .get(&lane)
        .map(|authors| authors.iter().any(|a| a == issuer))
        .unwrap_or(false)
    {
        return false;
    }
    true
}

/// Classify each (lane, cell) resolution of an artifact for the given lane.
fn classify_lane<'a>(
    artifact: &'a RunArtifact,
    lane: Lane,
    heads: &BTreeMap<ObservationId, Disposition>,
) -> Vec<(CellId, ObsClass)> {
    let mut out = Vec::new();
    for (key, resolution) in &artifact.grid {
        let (l, cell) = match parse_cell_key(key) {
            Some(x) => x,
            None => continue,
        };
        if l != lane {
            continue;
        }
        let class = match resolution {
            CellResolution::Coverage(_) => ObsClass::Coverage,
            CellResolution::Observed(o) => match &o.outcome {
                Outcome::Pass(_) => ObsClass::Pass,
                Outcome::Blocked { .. } => ObsClass::Blocked,
                Outcome::NotApplicable { .. } => ObsClass::Na,
                Outcome::Fail { .. } => match heads.get(&o.id) {
                    None => ObsClass::FailPending, // no valid head ⇒ product-counted.
                    Some(Disposition::Harness) => ObsClass::FailHarnessRemoved,
                    Some(Disposition::Product) => ObsClass::FailProduct,
                },
            },
        };
        out.push((cell, class));
    }
    out
}

/// The lane's gating cells (A5/A9): gating-dimension cells that APPLY to the lane
/// — the manifest marks them `Required` OR `CoveragePermitted`. A gating-dimension
/// cell marked `NaPermitted` is NOT in the set (the dimension genuinely does not
/// apply to the lane — the declared-n-a case, e.g. codex's D2). But a
/// `CoveragePermitted` gating cell DOES apply (it is runnable, just only on
/// another box), so it IS a gating requirement: resolving it by a coverage record
/// leaves it unmeasured on the evidence run → the gating claim fails →
/// experimental (A9: "a gating-dimension cell resolved by coverage record is
/// unmeasured → experimental, A5"). Excluding it here would silently let a
/// coverage-resolved gating cell pass the gating claim — the bug this closes.
fn lane_gating_cells(battery: &Battery, lane: Lane) -> BTreeSet<CellId> {
    let mut out = BTreeSet::new();
    for cell in &battery.cells {
        if cell.dimension.is_gating()
            && matches!(
                battery.applicability(lane, &cell.id),
                Applicability::Required | Applicability::CoveragePermitted
            )
        {
            out.insert(cell.id.clone());
        }
    }
    out
}

/// A5 gating: every gating cell PASS on the evidence run, AND no product/pending
/// gating fail anywhere in the window. On the evidence run only PASS satisfies —
/// blocked/na/coverage/harness-removed is unmeasured → the gating claim fails.
fn evaluate_gating(
    window: &[&Candidate],
    evrun: &Candidate,
    lane: Lane,
    gating_cells: &BTreeSet<CellId>,
    heads: &BTreeMap<ObservationId, Disposition>,
) -> bool {
    // gating_measured + every gating cell PASS on the evidence run.
    let ev_classes: BTreeMap<CellId, ObsClass> = classify_lane(&evrun.artifact, lane, heads)
        .into_iter()
        .collect();
    for cell in gating_cells {
        match ev_classes.get(cell) {
            Some(ObsClass::Pass) => {}
            _ => return false, // unmeasured or not PASS on the evidence run.
        }
    }
    // No product/pending gating fail anywhere in the window.
    for cand in window {
        for (cell, class) in classify_lane(&cand.artifact, lane, heads) {
            if gating_cells.contains(&cell)
                && matches!(class, ObsClass::FailProduct | ObsClass::FailPending)
            {
                return false;
            }
        }
    }
    true
}

/// A4 beta point-in-time: every applicable cell in the evidence run resolves to
/// exactly one of PASS | FAIL_* | BLOCKED(declared) | NA(declared) |
/// COVERAGE(declared) — zero undeclared absences; gaps named with re-entry.
///
/// **KEPT as documented DEFENSE-IN-DEPTH (fix-r7 decision).** Since fix-r6 moved
/// the declared-gap / well-formedness checks to a CORPUS-WIDE v5 (fullness +
/// non-empty Blocked/NA reasons + coverage well-formedness on every consumable
/// run), v5 now DOMINATES this predicate — a corpus that passed v5 always
/// satisfies beta_pt. I keep it rather than delete it so the code maps 1:1 to
/// A10's clause list (the doctrine oracle checks A10 clause by clause); it is the
/// spec's named beta point-in-time clause, evaluated honestly, now redundant with
/// v5's stronger corpus-wide enforcement.
fn beta_point_in_time(evrun: &Candidate, lane: Lane, battery: &Battery) -> bool {
    for cell in &battery.cells {
        if !battery.applicability(lane, &cell.id).in_domain() {
            continue;
        }
        let key = RunArtifact::cell_key(lane, &cell.id);
        match evrun.artifact.grid.get(&key) {
            None => return false, // undeclared absence.
            Some(CellResolution::Observed(o)) => match &o.outcome {
                Outcome::Blocked { reason, reentry } => {
                    if reason.trim().is_empty() || reentry.trim().is_empty() {
                        return false;
                    }
                }
                Outcome::NotApplicable { reason } => {
                    if reason.trim().is_empty() {
                        return false;
                    }
                }
                _ => {}
            },
            Some(CellResolution::Coverage(cov)) => {
                if cov.well_formed().is_err() {
                    return false;
                }
            }
        }
    }
    true
}

/// A4/A7 GA point-in-time: every applicable cell in the evidence run is PASS (only
/// PASS is green — harness-removed there is unmeasured, not green), every gating
/// cell automated-pass (A7), and evrun.commit == release_commit.
fn ga_point_in_time(
    evrun: &Candidate,
    lane: Lane,
    battery: &Battery,
    release_commit: &str,
) -> bool {
    if evrun.artifact.header.tuple.release_commit != release_commit {
        return false; // T2 evidence currency (the PARAMETER, F-3).
    }
    for cell in &battery.cells {
        let app = battery.applicability(lane, &cell.id);
        if !app.in_domain() {
            continue;
        }
        let key = RunArtifact::cell_key(lane, &cell.id);
        let resolution = match evrun.artifact.grid.get(&key) {
            Some(r) => r,
            None => return false,
        };
        match app {
            Applicability::Required => {
                // Only PASS is green; a coverage record / blocked / fail / na
                // fails GA's all-green clause.
                match resolution {
                    CellResolution::Observed(o) => {
                        if !matches!(o.outcome, Outcome::Pass(_)) {
                            return false;
                        }
                        // A7: gating cells automated-pass on the evidence run.
                        if cell.dimension.is_gating() && o.run_mode != RunMode::Automated {
                            return false;
                        }
                    }
                    CellResolution::Coverage(_) => return false,
                }
            }
            Applicability::NaPermitted { .. } => {
                // A declared NA is not a required-green cell; fine for GA.
                // But if the run resolved it to anything other than the permitted
                // NA (impossible past v5), it would already have failed v5.
            }
            Applicability::CoveragePermitted => {
                // Any applicable cell resolved by coverage record fails GA's
                // all-green point-in-time clause (A9).
                if matches!(resolution, CellResolution::Coverage(_)) {
                    return false;
                }
                // If resolved as an observation, it must be PASS to be green.
                if let CellResolution::Observed(o) = resolution {
                    if !matches!(o.outcome, Outcome::Pass(_)) {
                        return false;
                    }
                }
            }
            Applicability::OutOfDomain => {}
        }
    }
    true
}

// ===========================================================================
// (e) Provider badge = tier of the WEAKEST lane qd selects by default (T2).
// ===========================================================================

/// The default lanes qd selects for a provider. Each provider maps to the lane(s)
/// its default start resolves; the badge is the weakest of them.
pub fn provider_default_lanes(provider: &str) -> Vec<Lane> {
    match provider {
        "claude-code" => vec![Lane::ClaudeCode],
        "codex" => vec![Lane::Codex],
        "acp/claude-code" => vec![Lane::AcpClaudeCode],
        "acp/opencode" | "opencode" => vec![Lane::Opencode],
        "pi" => vec![Lane::Pi],
        _ => Vec::new(),
    }
}

/// The provider badge (e): the tier of the weakest lane qd selects by default for
/// the provider. An unknown provider or a provider with no default lanes has no
/// badge (None).
pub fn provider_badge(
    corpus: &Corpus,
    provider: &str,
    release_commit: &str,
    params: &TierParams,
) -> Option<Tier> {
    let lanes = provider_default_lanes(provider);
    if lanes.is_empty() {
        return None;
    }
    lanes
        .into_iter()
        .map(|lane| compute_tier(corpus, lane, release_commit, params).tier)
        .min()
}

// ===========================================================================
// Small helpers.
// ===========================================================================

/// Parse a grid cell key `"<provider-id>::<cell-id>"` back into (lane, cell).
fn parse_cell_key(key: &str) -> Option<(Lane, CellId)> {
    let (prov, cell) = key.split_once("::")?;
    let lane = Lane::ALL.into_iter().find(|l| l.provider_id() == prov)?;
    Some((lane, CellId(cell.to_string())))
}

/// Parse an RFC3339 UTC timestamp `YYYY-MM-DDTHH:MM:SS[.fff]Z` (or with a numeric
/// offset) into epoch seconds. A minimal parser — no chrono dependency. Returns
/// None on any shape it does not recognize.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    if bytes[4] != b'-' {
        return None;
    }
    let month: i64 = s.get(5..7)?.parse().ok()?;
    if bytes[7] != b'-' {
        return None;
    }
    let day: i64 = s.get(8..10)?.parse().ok()?;
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None;
    }
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    if bytes[13] != b':' {
        return None;
    }
    let min: i64 = s.get(14..16)?.parse().ok()?;
    if bytes[16] != b':' {
        return None;
    }
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    // Offset: trailing 'Z' → UTC; a +HH:MM / -HH:MM offset is subtracted.
    let mut offset_secs = 0i64;
    let rest = &s[19..];
    // skip fractional seconds
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if rest != "Z" && rest != "z" && !rest.is_empty() {
        // parse ±HH:MM
        let sign = match rest.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        offset_secs = sign * (oh * 3600 + om * 60);
    }
    // Days since epoch via a civil-from-days algorithm (Howard Hinnant).
    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + min * 60 + sec - offset_secs)
}

/// Days since 1970-01-01 for a proleptic Gregorian (y, m, d). Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
