//! Flake attribution as a typed, append-only, authority-bounded record (M-3,
//! head semantics F-2). C-1 makes two things **checkable** (A8's *authority*
//! bounds — issuer-not-executor/coordinator/lane-owner — are C-3's to enforce):
//!
//! 1. **Well-formedness** — every required field present (non-`Option`, so a
//!    missing field is unrepresentable), a *registered* reason code (an unknown
//!    code fails to parse — loudly invalid, never silently coerced), non-empty
//!    observation set and primary-evidence refs.
//! 2. **The unique active head per observation** — for every observation there
//!    is exactly one governing record or none. A record either roots an
//!    observation's chain (no live head) or names that observation's current
//!    head in `supersedes`. A record that would create a **second root** (covers
//!    a chained observation without superseding its head), a **fork**
//!    (supersedes a non-head), a **cycle** (supersedes a same-or-later record),
//!    or an **equal-sequence** ambiguity is **invalid: it never governs, never
//!    displaces any existing head, and is loudly flagged** — so laundering by
//!    contradiction is structurally impossible. Precedence is by
//!    attribution-sequence with record-id tie-break, **never** by issuer
//!    timestamp.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::ids::{AttributionSeq, ObservationId, RecordId, RunId};

/// Whether the ruling removes a fail from the rate (harness) or confirms it
/// (product). Attribution can only move a fail OUT (harness) or confirm it — it
/// can never convert a fail to a pass (A6/A8, enforced by C-3's classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    Harness,
    Product,
}

/// The small registered reason-code vocabulary. A closed enum: an unregistered
/// code cannot deserialize (serde errors), so a record with an off-vocabulary
/// reason is loudly invalid, never silently honored. Grows only by corpus change
/// (the link-tag rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasonCode {
    // Harness dispositions:
    /// Ambient dispatch env poisoned the harness (a known live class).
    HarnessNonhermeticEnv,
    /// A timing/ordering flake in the harness, not the product.
    HarnessFlakeTiming,
    /// Infrastructure outage unrelated to the product under test.
    HarnessInfraOutage,
    /// A defect in a test fixture or the harness itself.
    HarnessFixtureDefect,
    // Product disposition:
    /// A genuine product defect (confirms the fail).
    ProductDefect,
}

impl ReasonCode {
    /// The disposition a reason code is consistent with (a coherence aid; the
    /// record carries its own [`Disposition`], and C-3 enforces authority).
    pub fn consistent_with(self, d: Disposition) -> bool {
        match self {
            ReasonCode::ProductDefect => d == Disposition::Product,
            _ => d == Disposition::Harness,
        }
    }
}

/// A typed, append-only flake-attribution ruling. All fields required — a record
/// missing any is malformed and invalid by construction (there are no `Option`s
/// here). Corrections are **new records superseding old ones**, never edits,
/// never deletions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionRecord {
    pub id: RecordId,
    /// Authority-issued, monotonic per the issuance register; precedence key.
    pub sequence: AttributionSeq,
    pub run: RunId,
    /// The immutable observation ids this record rules on (non-empty).
    pub observations: Vec<ObservationId>,
    pub disposition: Disposition,
    pub reason: ReasonCode,
    /// Primary-evidence refs (non-empty).
    pub primary_evidence: Vec<String>,
    /// Issuer identity (instance name + commission id) — the audit surface A8
    /// (C-3) checks against the run's roles.
    pub issuer: String,
    /// Descriptive only — never a precedence input.
    pub timestamp: String,
    /// The current head(s) this record supersedes (a set, when covered
    /// observations have different heads). Empty when it roots every chain.
    pub supersedes: Vec<RecordId>,
}

impl AttributionRecord {
    /// Structural well-formedness (the non-`Option` fields are already enforced
    /// by the type; this catches the emptiness the type cannot): non-empty
    /// observation set, non-empty evidence, and disposition/reason coherence.
    pub fn well_formed(&self) -> Result<(), String> {
        if self.observations.is_empty() {
            return Err(format!("record {:?}: empty observation set", self.id.0));
        }
        if self.primary_evidence.iter().all(|e| e.trim().is_empty()) {
            return Err(format!("record {:?}: no primary-evidence refs", self.id.0));
        }
        if !self.reason.consistent_with(self.disposition) {
            return Err(format!(
                "record {:?}: reason {:?} inconsistent with disposition {:?}",
                self.id.0, self.reason, self.disposition
            ));
        }
        Ok(())
    }

    /// Precedence key: (sequence, record-id) — never timestamp.
    fn precedence(&self) -> (u64, &str) {
        (self.sequence.get(), &self.id.0)
    }
}

/// Why a record was rejected as invalid (loudly flagged; it never governs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvalidRecord {
    pub id: RecordId,
    pub reason: String,
}

/// The resolved chain state for a run's observations: the unique active head per
/// observation, and every record rejected as invalid (never a silent drop).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ChainResolution {
    /// observation → the record id that is its unique active head.
    pub heads: BTreeMap<ObservationId, RecordId>,
    /// Records rejected as invalid, with the reason each was rejected.
    pub invalid: Vec<InvalidRecord>,
}

impl ChainResolution {
    pub fn head_of(&self, obs: &ObservationId) -> Option<&RecordId> {
        self.heads.get(obs)
    }
}

/// Resolve the unique active head per observation from an append-only set of
/// attribution records (F-2). Total and deterministic: every record is either a
/// governing head-update or an [`InvalidRecord`] with a reason; contradictory
/// records never both govern.
///
/// Processing order is by precedence `(sequence, record-id)`. A record is
/// applied only if, for the covered observations, its `supersedes` set is
/// **exactly** the set of those observations' current heads — no missing head (a
/// second root), no extra id (a fork), no same-or-later supersede (a cycle /
/// forward reference), and its sequence is unique. An invalid record leaves all
/// existing heads in force.
pub fn resolve_heads(records: &[AttributionRecord]) -> ChainResolution {
    let mut res = ChainResolution::default();

    // Equal-sequence ambiguity: any two records sharing a sequence are both
    // invalid (the sequence is authority-issued and unique by contract).
    let mut seq_counts: BTreeMap<u64, usize> = BTreeMap::new();
    for r in records {
        *seq_counts.entry(r.sequence.get()).or_default() += 1;
    }

    // Order candidates by precedence; drop malformed and equal-sequence up front.
    let mut ordered: Vec<&AttributionRecord> = Vec::new();
    for r in records {
        if let Err(e) = r.well_formed() {
            res.invalid.push(InvalidRecord {
                id: r.id.clone(),
                reason: format!("malformed: {e}"),
            });
            continue;
        }
        if seq_counts.get(&r.sequence.get()).copied().unwrap_or(0) > 1 {
            res.invalid.push(InvalidRecord {
                id: r.id.clone(),
                reason: format!(
                    "equal-sequence ambiguity: sequence {} is shared by another record",
                    r.sequence.get()
                ),
            });
            continue;
        }
        ordered.push(r);
    }
    ordered.sort_by(|a, b| a.precedence().cmp(&b.precedence()));

    // Apply in precedence order, maintaining per-observation head + head sequence.
    let mut head_seq: BTreeMap<ObservationId, u64> = BTreeMap::new();
    for r in ordered {
        let s: BTreeSet<&str> = r.supersedes.iter().map(|id| id.0.as_str()).collect();

        // Cycle / forward-reference: a superseded record must have strictly lower
        // precedence (it must already be a head we have seen).
        let mut bad = None;
        for sup in &r.supersedes {
            match res
                .heads
                .iter()
                .find(|(_, hid)| **hid == *sup)
                .map(|(obs, _)| head_seq.get(obs).copied().unwrap_or(0))
            {
                Some(sup_seq) if sup_seq >= r.sequence.get() => {
                    bad = Some(format!(
                        "cycle/forward reference: supersedes {:?} at sequence {} >= own sequence {}",
                        sup.0, sup_seq, r.sequence.get()
                    ));
                    break;
                }
                None => {
                    // supersedes an id that is not any observation's current head.
                    bad = Some(format!(
                        "fork: supersedes {:?}, which is not a current head",
                        sup.0
                    ));
                    break;
                }
                _ => {}
            }
        }
        if let Some(reason) = bad {
            res.invalid.push(InvalidRecord {
                id: r.id.clone(),
                reason,
            });
            continue;
        }

        // Required supersede set = the current heads of covered observations that
        // already have one. Second root = a covered observation whose head is not
        // superseded. Fork = a superseded id that is no covered observation's head.
        let mut required: BTreeSet<&str> = BTreeSet::new();
        let mut second_root = None;
        for obs in &r.observations {
            if let Some(h) = res.heads.get(obs) {
                required.insert(h.0.as_str());
                if !s.contains(h.0.as_str()) {
                    second_root = Some(format!(
                        "second root: observation {:?} already has head {:?}, not superseded",
                        obs.0, h.0
                    ));
                    break;
                }
            }
        }
        if let Some(reason) = second_root {
            res.invalid.push(InvalidRecord {
                id: r.id.clone(),
                reason,
            });
            continue;
        }
        if let Some(extra) = s.difference(&required).next() {
            res.invalid.push(InvalidRecord {
                id: r.id.clone(),
                reason: format!(
                    "fork: supersedes {:?}, which is not a current head of any covered observation",
                    extra
                ),
            });
            continue;
        }

        // Valid: this record becomes the unique active head for every covered
        // observation (rooting those without one, displacing those it superseded).
        for obs in &r.observations {
            res.heads.insert(obs.clone(), r.id.clone());
            head_seq.insert(obs.clone(), r.sequence.get());
        }
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(id: &str) -> ObservationId {
        ObservationId(id.into())
    }

    /// A well-formed harness-disposition record over `observations`, superseding
    /// `supersedes`, at sequence `seq`. Coherent by default (Harness + a harness
    /// reason + non-empty evidence) so tests vary exactly one axis at a time.
    fn rec(id: &str, seq: u64, observations: &[&str], supersedes: &[&str]) -> AttributionRecord {
        AttributionRecord {
            id: RecordId(id.into()),
            sequence: AttributionSeq::new(seq),
            run: RunId("run-1".into()),
            observations: observations.iter().map(|o| obs(o)).collect(),
            disposition: Disposition::Harness,
            reason: ReasonCode::HarnessFlakeTiming,
            primary_evidence: vec!["evidence-ref-1".into()],
            issuer: "attributor-1 (uq5xx83a)".into(),
            timestamp: "2026-07-15T00:00:00Z".into(),
            supersedes: supersedes.iter().map(|s| RecordId((*s).into())).collect(),
        }
    }

    #[test]
    fn well_formed_rejects_empty_observation_set() {
        let r = rec("rec-1", 1, &[], &[]);
        let err = r.well_formed().unwrap_err();
        assert!(err.contains("empty observation set"), "{err}");
    }

    #[test]
    fn well_formed_rejects_empty_evidence() {
        let mut r = rec("rec-1", 1, &["O1"], &[]);
        r.primary_evidence = vec!["   ".into()];
        let err = r.well_formed().unwrap_err();
        assert!(err.contains("primary-evidence"), "{err}");
    }

    #[test]
    fn well_formed_rejects_reason_disposition_incoherence() {
        let mut r = rec("rec-1", 1, &["O1"], &[]);
        // A harness reason on a product disposition is incoherent.
        r.disposition = Disposition::Product;
        let err = r.well_formed().unwrap_err();
        assert!(err.contains("inconsistent"), "{err}");
    }

    #[test]
    fn resolve_heads_drops_a_malformed_record_loudly() {
        // A malformed record never governs and is flagged (never silently dropped).
        let bad = rec("rec-bad", 1, &[], &[]);
        let res = resolve_heads(&[bad]);
        assert!(res.heads.is_empty());
        assert!(
            res.invalid
                .iter()
                .any(|i| i.id.0 == "rec-bad" && i.reason.contains("malformed")),
            "{:?}",
            res.invalid
        );
    }

    #[test]
    fn a_chained_supersede_updates_the_head_and_both_are_valid() {
        let r1 = rec("rec-1", 1, &["O1"], &[]);
        let r2 = rec("rec-2", 2, &["O1"], &["rec-1"]);
        let res = resolve_heads(&[r1, r2]);
        assert!(res.invalid.is_empty(), "{:?}", res.invalid);
        assert_eq!(res.head_of(&obs("O1")).unwrap().0, "rec-2");
    }

    #[test]
    fn a_second_root_never_displaces_the_existing_head() {
        let r1 = rec("rec-1", 1, &["O1"], &[]);
        // Covers O1 (already headed by rec-1) but supersedes nothing — a second root.
        let r2 = rec("rec-2", 2, &["O1"], &[]);
        let res = resolve_heads(&[r1, r2]);
        assert_eq!(res.head_of(&obs("O1")).unwrap().0, "rec-1");
        assert!(
            res.invalid
                .iter()
                .any(|i| i.id.0 == "rec-2" && i.reason.contains("second root")),
            "{:?}",
            res.invalid
        );
    }

    #[test]
    fn a_supersede_of_a_non_head_is_a_fork_and_invalid() {
        let r1 = rec("rec-1", 1, &["O1"], &[]);
        // Supersedes an id that is no observation's current head.
        let r2 = rec("rec-2", 2, &["O1"], &["ghost"]);
        let res = resolve_heads(&[r1, r2]);
        assert_eq!(res.head_of(&obs("O1")).unwrap().0, "rec-1");
        assert!(
            res.invalid
                .iter()
                .any(|i| i.id.0 == "rec-2" && i.reason.contains("fork")),
            "{:?}",
            res.invalid
        );
    }

    #[test]
    fn a_forward_reference_supersede_is_invalid() {
        // rec-2 supersedes rec-3, which has a HIGHER sequence and is not yet a
        // head when rec-2 is processed — a forward reference. It surfaces as a
        // fork ("not a current head"), which is the reachable manifestation of a
        // forward/cyclic supersede once equal-sequence ambiguity is pre-filtered.
        let r1 = rec("rec-1", 1, &["O1"], &[]);
        let r2 = rec("rec-2", 2, &["O1"], &["rec-3"]);
        let r3 = rec("rec-3", 3, &["O1"], &["rec-1"]);
        let res = resolve_heads(&[r1, r2, r3]);
        assert!(
            res.invalid.iter().any(|i| i.id.0 == "rec-2"),
            "forward-referencing rec-2 must be invalid: {:?}",
            res.invalid
        );
        // rec-3 validly supersedes the real head rec-1.
        assert_eq!(res.head_of(&obs("O1")).unwrap().0, "rec-3");
    }

    #[test]
    fn equal_sequence_records_never_govern() {
        // Two records sharing a sequence are BOTH invalid — the sequence is
        // authority-issued and unique by contract; a collision is ambiguity.
        let r1 = rec("rec-1", 5, &["O1"], &[]);
        let r2 = rec("rec-2", 5, &["O2"], &[]);
        let res = resolve_heads(&[r1, r2]);
        assert!(res.heads.is_empty(), "neither may govern: {:?}", res.heads);
        assert!(res
            .invalid
            .iter()
            .any(|i| i.id.0 == "rec-1" && i.reason.contains("equal-sequence")));
        assert!(res
            .invalid
            .iter()
            .any(|i| i.id.0 == "rec-2" && i.reason.contains("equal-sequence")));
    }
}
