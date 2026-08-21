//! C-3 (f) — **T4, the public tier document**. A repo artifact a stranger who
//! has never seen this ledger can act on: what the tiers mean; the current lane
//! assignments with evidence links (each citing its six context inputs, "current
//! as of" the anchoring snapshot's attested time); the designated evidence box
//! per lane with the full append-only designation history (pending replacement,
//! its effective-after boundary, its accumulation state, and the derived
//! promotion ordinal once earned); and how a stranger re-runs the battery on
//! their own box.
//!
//! **Interpretation fidelity is VISIBLE in the artifact itself** (QS-4): (i)
//! lanes not providers, (ii) window not single-run, (iii) demotes not blocks.
//! Honest staleness is rendered, never hidden.
//!
//! The document is a pure function of the corpus + release commit + gated params
//! — a fresh reader who recomputes the tiers gets the identical table, so the
//! prose here never asserts anything the computation did not derive.

use serde::{Deserialize, Serialize};

use super::ids::{ArtifactDigest, Lane};
use super::tier::Corpus;
use super::tier::{
    compute_tier, provider_default_lanes, Citation, RateValue, Tier, TierComputation, TierParams,
    TierVerdict,
};

/// The lanes T4 publishes, in a stable published order.
const PUBLISHED_LANES: [Lane; 5] = [
    Lane::ClaudeCode,
    Lane::Codex,
    Lane::AcpClaudeCode,
    Lane::Opencode,
    Lane::Pi,
];

/// The providers T4 badges, in a stable order.
const PUBLISHED_PROVIDERS: [&str; 5] = [
    "claude-code",
    "codex",
    "acp/claude-code",
    "acp/opencode",
    "pi",
];

// ===========================================================================
// The publication layer (A10 v7, the two obligations that are NOT per-corpus):
// a CURRENT publication anchors at the registrar's LATEST snapshot, and
// NO-ROLLBACK — a published row's snapshot ordinal ≥ every previously-published
// row's for the table. Both compare against state outside `tier()`'s signature
// (the registrar's latest snapshot + T4's append-only published-row history), so
// they live here, in the publication layer, not in `tier()` (R6-5).
// ===========================================================================

/// Whether a publication is CURRENT (anchors at the registrar's latest snapshot,
/// subject to no-rollback) or an explicitly HISTORICAL rendering (legally anchored
/// at an older snapshot, exempt from no-rollback but marked as historical — never
/// mistakable for the current tier state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PublicationMode {
    Current,
    Historical,
}

/// One append-only published-row record — the operand no-rollback reads. The
/// history is per TABLE (the tier document); every lane row in one publication
/// shares its anchoring snapshot ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedRow {
    pub snapshot_ordinal: u64,
    pub snapshot_attested_time: String,
    pub release_commit: String,
    pub mode: PublicationMode,
}

/// T4's append-only publication history — the durable record no-rollback checks a
/// new CURRENT publication against (R6-5: "checkable from T4's append-only
/// history, not merely visible"). Never mutated except by appends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationHistory {
    pub rows: Vec<PublishedRow>,
}

impl PublicationHistory {
    /// The highest snapshot ordinal among previously-published CURRENT rows — the
    /// floor a new current publication must not fall below. Historical renderings
    /// never advance (or gate) this — they are explicitly backward looks.
    pub fn current_high_water(&self) -> Option<u64> {
        self.rows
            .iter()
            .filter(|r| r.mode == PublicationMode::Current)
            .map(|r| r.snapshot_ordinal)
            .max()
    }
}

/// The registrar's latest-snapshot **attestation** — the authority record that
/// claims CURRENCY (R6-5: "the registrar's snapshot attestation is what claims
/// currency"). It is an explicit external input to the publish path (NOT a
/// live-only signal): the fresh-reader scenario carries it exactly as it carries
/// the release commit and the anchoring snapshot. A CURRENT publication is valid
/// only when the corpus's own anchoring snapshot IS this attestation — same
/// ordinal (currency: no anchoring at a stale intermediate snapshot while a
/// fresher one exists), same attested head digest and observed time (integrity:
/// the anchored snapshot is genuinely the registrar's, not a look-alike at the
/// right ordinal). Mirrors the shape of a `PublicationSnapshotEvent` + its ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrarAttestation {
    pub latest_ordinal: u64,
    pub attested_head_digest: ArtifactDigest,
    pub observed_at: String,
}

/// Why a publication was rejected (invalid by construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The corpus is not anchored at any publication-snapshot (v7 anchoring).
    NotAnchored,
    /// A CURRENT publication does not anchor at the REGISTRAR'S latest snapshot
    /// (currency, obligation (2)) — the corpus's anchoring snapshot differs from
    /// the registrar's attestation (a stale intermediate anchor, or a look-alike).
    NotAnchoredAtLatest { anchor: u64, latest: u64 },
    /// NO-ROLLBACK: a CURRENT publication's snapshot ordinal is below a
    /// previously-published current row's — an earlier-anchored refresh is INVALID
    /// (R6-5), never merely "historical-by-accident".
    RollBack {
        new_ordinal: u64,
        prior_ordinal: u64,
    },
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::NotAnchored => {
                write!(f, "publication invalid: corpus is not anchored at any publication-snapshot (v7)")
            }
            PublishError::NotAnchoredAtLatest { anchor, latest } => write!(
                f,
                "publication invalid: a CURRENT publication anchored at snapshot ordinal {anchor} is not the REGISTRAR'S latest snapshot ({latest}) (v7 — a current publication must anchor at the registrar's latest, never a stale intermediate)"
            ),
            PublishError::RollBack { new_ordinal, prior_ordinal } => write!(
                f,
                "publication invalid: NO-ROLLBACK — a CURRENT publication anchored at snapshot ordinal {new_ordinal} is below a previously-published current row's ordinal {prior_ordinal} (v7/R6-5); an earlier-anchored refresh is INVALID, not merely historical"
            ),
        }
    }
}

/// Publish the tier document (the T4-layer half of v7). For a CURRENT publication:
/// verifies the corpus anchors at the REGISTRAR'S latest snapshot (currency —
/// obligation (2), against the external `registrar` attestation) AND enforces
/// no-rollback against `history` (the new snapshot ordinal must be ≥ every prior
/// CURRENT row's), then APPENDS the new row to `history` and renders. For a
/// HISTORICAL rendering: exempt from BOTH (it is an explicit backward look) but
/// rendered with a HISTORICAL banner and recorded as historical (never advancing
/// the current high-water). Returns the rendered document or the publication error
/// that makes it invalid by construction.
///
/// **Obligation (2) needs the registrar attestation because it is NOT decidable
/// from the corpus alone** — a truncated prefix's own latest snapshot IS its
/// anchor, so a corpus-internal `anchor == latest` check is a tautology. The
/// currency it enforces (no publishing a stale intermediate snapshot as "current"
/// while a fresher registrar snapshot exists) can only be judged against the
/// registrar's out-of-corpus record, supplied here as `registrar`.
pub fn publish_tier_document(
    corpus: &Corpus,
    release_commit: &str,
    params: &TierParams,
    mode: PublicationMode,
    registrar: &RegistrarAttestation,
    history: &mut PublicationHistory,
) -> Result<String, PublishError> {
    let (anchor_ord, anchor_snap) = match corpus.journal.latest_snapshot() {
        Some((o, s)) => (o.get(), s.clone()),
        None => return Err(PublishError::NotAnchored),
    };
    let attested = anchor_snap.observed_at.clone();
    // Obligation (2), currency (R6-5): a CURRENT publication must anchor at the
    // REGISTRAR'S latest snapshot — same ordinal, same attested head + time. The
    // comparison is against the external attestation, not against the corpus's own
    // latest (which for a truncated prefix is itself — the tautology this closes).
    if mode == PublicationMode::Current
        && (anchor_ord != registrar.latest_ordinal
            || anchor_snap.artifact_index_head_digest != registrar.attested_head_digest
            || anchor_snap.observed_at != registrar.observed_at)
    {
        return Err(PublishError::NotAnchoredAtLatest {
            anchor: anchor_ord,
            latest: registrar.latest_ordinal,
        });
    }
    if mode == PublicationMode::Current {
        if let Some(prior) = history.current_high_water() {
            if anchor_ord < prior {
                return Err(PublishError::RollBack {
                    new_ordinal: anchor_ord,
                    prior_ordinal: prior,
                });
            }
        }
    }
    let doc = render_tier_document_with_mode(corpus, release_commit, params, mode);
    // Append the published row (append-only; the ledger only ever grows).
    history.rows.push(PublishedRow {
        snapshot_ordinal: anchor_ord,
        snapshot_attested_time: attested,
        release_commit: release_commit.to_string(),
        mode,
    });
    Ok(doc)
}

/// Render the T4 public tier document as Markdown from `corpus` at
/// `release_commit` under `params`, as a CURRENT publication. Deterministic: two
/// readers of the same inputs produce a byte-identical document. For the full
/// publication check (no-rollback, current-anchors-at-latest) use
/// [`publish_tier_document`]; this renders the body only.
pub fn render_tier_document(corpus: &Corpus, release_commit: &str, params: &TierParams) -> String {
    render_tier_document_with_mode(corpus, release_commit, params, PublicationMode::Current)
}

/// Render the document in an explicit [`PublicationMode`] — a HISTORICAL rendering
/// carries a visible banner so it can never be mistaken for the current tier state.
pub fn render_tier_document_with_mode(
    corpus: &Corpus,
    release_commit: &str,
    params: &TierParams,
    mode: PublicationMode,
) -> String {
    let mut out = String::new();
    let computations: Vec<(Lane, TierComputation)> = PUBLISHED_LANES
        .iter()
        .map(|&lane| (lane, compute_tier(corpus, lane, release_commit, params)))
        .collect();

    // The anchoring snapshot's attested time is the document's "current as of".
    let current_as_of = computations
        .iter()
        .find_map(|(_, c)| c.citation.snapshot_attested_time.clone())
        .unwrap_or_else(|| "(no anchoring publication-snapshot — evidence not current)".into());

    header(&mut out, release_commit, &current_as_of, params, mode);
    what_tiers_mean(&mut out, params);
    interpretation_fidelity(&mut out);
    current_assignments(&mut out, &computations);
    provider_badges(&mut out, corpus, release_commit, params);
    designation_boxes(&mut out, &computations, params);
    citations_section(&mut out, &computations);
    how_to_rerun(&mut out);

    out
}

fn header(
    out: &mut String,
    release_commit: &str,
    current_as_of: &str,
    params: &TierParams,
    mode: PublicationMode,
) {
    out.push_str("# qd provider-conformance tiers\n\n");
    if mode == PublicationMode::Historical {
        // A HISTORICAL rendering must never be mistaken for the current state
        // (v7): it is an explicit backward look at an older anchoring snapshot.
        out.push_str(&format!(
            "> ⚠️ **HISTORICAL RENDERING — NOT THE CURRENT TIER STATE.** This document is anchored\n\
             > at an OLDER publication-snapshot (attested {current_as_of}) and is exempt from the\n\
             > no-rollback rule precisely because it does not claim to be current. The live tier\n\
             > state is whatever the most recent CURRENT publication reports.\n\n"
        ));
    }
    out.push_str(
        "This document reports, for each **provider lane**, the conformance tier qd's evidence\n\
         supports — computed deterministically from a recorded evidence corpus by the preregistered\n\
         tier scheme (A1–A10). A fresh reader who re-runs the computation over the same corpus gets\n\
         the identical table; nothing here is asserted by hand.\n\n",
    );
    out.push_str(&format!("- **Release commit:** `{release_commit}`\n"));
    let as_of_label = match mode {
        PublicationMode::Current => "Current as of",
        PublicationMode::Historical => "Historical rendering as of",
    };
    out.push_str(&format!(
        "- **{as_of_label}:** {current_as_of} (the anchoring publication-snapshot's attested time)\n"
    ));
    out.push_str(&format!(
        "- **Gated parameters:** window W={}, beta ≥ {}%, GA ≥ {}%, run-liveness deadline Δ={}h\n\n",
        params.window,
        params.beta_pct,
        params.ga_pct,
        params.delta_secs / 3600
    ));
}

fn what_tiers_mean(out: &mut String, params: &TierParams) {
    out.push_str("## What the tiers mean\n\n");
    out.push_str(&format!(
        "- **experimental** — the birth tier. Either the gating dimensions are unmeasured/blocked,\n\
          or the trailing window of {W} eligible runs has not accumulated, or the aggregate is below\n\
          beta. No lane is born beta by assertion.\n",
        W = params.window
    ));
    out.push_str(&format!(
        "- **beta** — the gating dimensions (D1 teardown, D2 receipts, D6 failure-honesty) are green\n\
          on the evidence run, every other applicable cell is measured or a named gap with a re-entry\n\
          condition, and the pooled pass-rate is ≥ {beta}% over the trailing window.\n",
        beta = params.beta_pct
    ));
    out.push_str(&format!(
        "- **GA** — every applicable cell is green on the evidence run (including D7 for attach\n\
          lanes), the gating cells passed **automated** (no scripted-manual), there are zero\n\
          undeclared not-runs, the pooled pass-rate is ≥ {ga}% over the window, and the evidence is\n\
          at the current release commit.\n\n",
        ga = params.ga_pct
    ));
    out.push_str(
        "Where a corpus fails staged validation, the lane reports **experimental — invalid evidence**\n\
         with the failed checks named: an unverifiable corpus never yields a favourable tier.\n\n",
    );
}

fn interpretation_fidelity(out: &mut String) {
    // The three adopted interpretations, rendered VISIBLE per QS-4.
    out.push_str("## How to read this table (interpretation fidelity)\n\n");
    out.push_str(
        "1. **Lanes, not providers.** A tier is a property of a *provider lane* (e.g. `codex`,\n\
            `acp/claude-code`), measured on one designated evidence box — not a blanket claim about a\n\
            provider across every configuration. A provider *badge* (below) is the weakest of the\n\
            lanes qd selects by default for that provider.\n",
    );
    out.push_str(
        "2. **A window, not a single run.** A tier rests on the trailing window of eligible runs\n\
            (pooled pass-rate), plus point-in-time predicates on the most recent (evidence) run — never\n\
            a single lucky green run.\n",
    );
    out.push_str(
        "3. **A red cell demotes; it does not block.** A failing cell lowers the tier (it counts\n\
            toward the aggregate and can break a gating claim). It never blocks the battery or the\n\
            provider — the provider still runs; its tier is just honest about what failed.\n\n",
    );
}

fn current_assignments(out: &mut String, computations: &[(Lane, TierComputation)]) {
    out.push_str("## Current lane assignments\n\n");
    out.push_str("| Lane | Tier | Pass-rate (window) | Evidence run | Notes |\n");
    out.push_str("|------|------|--------------------|--------------|-------|\n");
    for (lane, c) in computations {
        let tier = tier_label(&c.tier);
        let rate = c
            .rate
            .as_ref()
            .map(render_rate)
            .unwrap_or_else(|| "—".into());
        let evrun = c
            .evidence_run
            .as_ref()
            .map(|r| format!("`{}`", r.0))
            .unwrap_or_else(|| "—".into());
        let mut note = verdict_note(&c.verdict);
        if c.stale {
            note.push_str(" ⚠ stale (evidence not at current release commit)");
        }
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            lane.provider_id(),
            tier,
            rate,
            evrun,
            note
        ));
    }
    out.push('\n');
}

fn provider_badges(out: &mut String, corpus: &Corpus, release_commit: &str, params: &TierParams) {
    out.push_str("## Provider badges (weakest default lane)\n\n");
    out.push_str(
        "A provider's badge is the tier of the **weakest lane qd selects by default** for it.\n\n",
    );
    out.push_str("| Provider | Badge | Default lane(s) |\n");
    out.push_str("|----------|-------|-----------------|\n");
    for provider in PUBLISHED_PROVIDERS {
        let lanes = provider_default_lanes(provider);
        let badge = super::tier::provider_badge(corpus, provider, release_commit, params)
            .map(|t| tier_label(&t))
            .unwrap_or_else(|| "—".into());
        let lane_list = lanes
            .iter()
            .map(|l| format!("`{}`", l.provider_id()))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("| `{provider}` | {badge} | {lane_list} |\n"));
    }
    out.push('\n');
}

fn designation_boxes(
    out: &mut String,
    computations: &[(Lane, TierComputation)],
    params: &TierParams,
) {
    out.push_str("## Designated evidence box, per lane\n\n");
    out.push_str(
        "Each lane's published tier reads exactly one **designated evidence box**. Cross-box evidence\n\
         is corroborating information, never official-window input. Designation is governed and\n\
         outcome-independent — never chosen for a favourable result.\n\n",
    );
    for (lane, c) in computations {
        out.push_str(&format!("### `{}`\n\n", lane.provider_id()));
        match &c.active_box {
            Some(b) => out.push_str(&format!("- **Active evidence box:** `{b}`\n")),
            None => out.push_str("- **Active evidence box:** none designated yet\n"),
        }
        match (&c.pending_box, c.pending_boundary) {
            (Some(pb), Some(boundary)) => {
                out.push_str(&format!(
                    "- **Pending replacement:** `{pb}` — effective-after boundary at journal ordinal {boundary}; \
                     runs on `{pb}` after that ordinal accumulate toward promotion while the active box keeps governing the tier.\n"
                ));
            }
            _ => out.push_str("- **Pending replacement:** none\n"),
        }
        match c.promotion_ordinal {
            Some(p) => out.push_str(&format!(
                "- **Derived promotion:** the pending box promoted automatically at journal ordinal {p} (the Wth eligible pending-box run's terminal ordinal — replay arithmetic, never an authority action).\n"
            )),
            None => out.push_str(&format!(
                "- **Derived promotion:** not yet promoted (accumulation state: {}/{} eligible pending-box runs)\n",
                c.accumulation, params.window
            )),
        }
        // Full append-only designation history — T4 publishes the record, not just
        // the current state.
        if c.designation_history.is_empty() {
            out.push_str("- **Designation history:** none recorded\n");
        } else {
            out.push_str("- **Designation history (append-only):**\n");
            for h in &c.designation_history {
                out.push_str(&format!(
                    "  - ordinal {}: {} → box `{}` — basis: {} — {} (by {}, {})\n",
                    h.ordinal,
                    h.kind,
                    h.box_id,
                    h.policy_basis,
                    h.rationale,
                    h.authorized_by,
                    h.timestamp
                ));
            }
        }
        out.push('\n');
    }
}

fn citations_section(out: &mut String, computations: &[(Lane, TierComputation)]) {
    out.push_str("## Evidence citations (the six context inputs)\n\n");
    out.push_str(
        "Every tier claim above cites the exact inputs it was computed from. A claim with no\n\
         citation is invalid by construction; a claim whose evidence commit is not the current\n\
         release commit renders **stale**.\n\n",
    );
    for (lane, c) in computations {
        out.push_str(&format!("### `{}`\n\n", lane.provider_id()));
        render_citation(out, &c.citation);
        out.push('\n');
    }
}

fn render_citation(out: &mut String, cit: &Citation) {
    out.push_str(&format!("- Release commit: `{}`\n", cit.release_commit));
    out.push_str(&format!(
        "- Manifest digest: {}\n",
        cit.manifest_digest
            .as_ref()
            .map(|d| format!("`{}`", d.0))
            .unwrap_or_else(|| "— (context missing at release commit)".into())
    ));
    out.push_str(&format!(
        "- Aggregation version: {}\n",
        cit.aggregation_version
            .as_ref()
            .map(|v| format!("`{}`", v.0))
            .unwrap_or_else(|| "—".into())
    ));
    out.push_str(&format!(
        "- Authority-journal version: `{}` @ high-water sequence {}\n",
        cit.journal_content_digest.0, cit.journal_high_water
    ));
    out.push_str(&format!(
        "- Artifact-index head digest: `{}`\n",
        cit.index_head_digest.0
    ));
    match (cit.snapshot_ordinal, &cit.snapshot_attested_time) {
        (Some(o), Some(t)) => out.push_str(&format!(
            "- Anchoring publication-snapshot: ordinal {o}, attested {t}\n"
        )),
        _ => {
            out.push_str("- Anchoring publication-snapshot: — (not anchored — invalid evidence)\n")
        }
    }
}

fn how_to_rerun(out: &mut String) {
    out.push_str("## Re-running the battery on your own box\n\n");
    out.push_str(
        "The tiers above are reproducible. To reproduce them, or to measure a lane on your own box:\n\n\
         1. Check out the repo at the release commit cited above.\n\
         2. Build the conformance battery: the manifest (cell set + per-lane applicability) and the\n\
            aggregation-version artifact live in the dispatch tree at that commit, so the pinned\n\
            commit fixes both — no ambient state.\n\
         3. Run the battery for the lane you care about, on the lane's designated evidence box, as an\n\
            `evidence`-kind run commissioned through the authority journal (a run must be registered\n\
            **before** it executes, or it is ineligible). An `exercise`-kind run never enters a window.\n\
         4. Publish the run's artifact; the completion event cites its content digest, and a\n\
            publication-snapshot anchors the corpus.\n\
         5. Recompute: `tier(lane, corpus, release_commit, {W=3, beta=0.80, ga=0.95, Δ=24h})`. Two\n\
            readers of the same corpus get a byte-identical table.\n\n\
         Your box's grid is corroborating information. It becomes official-window input only if your\n\
         box is the lane's **designated** evidence box (a governed, outcome-independent choice).\n\n",
    );
}

// ---- small renderers -------------------------------------------------------

fn tier_label(t: &Tier) -> String {
    match t {
        Tier::Ga => "**GA**".into(),
        Tier::Beta => "beta".into(),
        Tier::Experimental => "experimental".into(),
    }
}

fn render_rate(r: &RateValue) -> String {
    match r {
        RateValue::Rate { passes, pool } => {
            if *pool == 0 {
                "UNAVAILABLE".into()
            } else {
                // integer percent, rounded down, plus the raw fraction.
                format!("{}% ({}/{})", passes * 100 / pool, passes, pool)
            }
        }
        RateValue::Unavailable => "UNAVAILABLE".into(),
    }
}

fn verdict_note(v: &TierVerdict) -> String {
    match v {
        TierVerdict::Ga => "all applicable cells green on the evidence run".into(),
        TierVerdict::Beta => "gating green; aggregate ≥ beta over the window".into(),
        TierVerdict::Experimental { label } => label.clone(),
        TierVerdict::Suppressed { label, entries } => format!(
            "{label} — unresolved: {}",
            entries
                .iter()
                .map(|e| e.0.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TierVerdict::InvalidEvidence { failed_checks } => {
            format!("invalid evidence: {}", failed_checks.join("; "))
        }
    }
}
