//! The authority journal (F-3/N-1/N-2, R6-1/R6-5/R6-6, R7-2, R5-O7). **One**
//! append-only artifact holding ALL SIX event kinds — run-commissioning,
//! run-start/token-redemption, run-terminal transitions, designation,
//! context-activation, and publication-snapshot — under one strictly-increasing,
//! unique ordinal. Because every event kind shares the one ordinal domain,
//! designation pre-outcome standing (A9) — "only runs commissioned AFTER the
//! designation event count toward the new box's window" — is a pure integer
//! comparison `run.sequence > designation.ordinal`, with no cross-register
//! reconciliation (N-2, the operand Sol found missing in R4).
//!
//! **C-1's scope here** is the journal's OWN well-formedness, enforced at append
//! AND checkable at parse: sequences strictly increasing and unique, run ids
//! unique, no replayed entries, each run-terminal transition recorded as its own
//! appended, ordinal-bearing event (at most one per run, its ordinal strictly
//! after the commissioning and run-start ordinals — never an in-place field
//! mutation), a void/abort ruling carrying the outcome-independent evidence its
//! validity needs, at most one run-start per entry with a runner identity
//! distinct from the commissioning identity (for evidence-kind runs), and a
//! unique, non-empty launch nonce per run-start. The **corpus-scope** checks
//! that consume this — A10 v1's journal-vs-corpus integrity, v4's
//! terminal-completeness across the assembled corpus, v8's nonce-uniqueness
//! corpus-wide, and the designation replay — live inside A10 (C-3). This module
//! hands C-3 a well-formed journal + `integrity_check`; it does not compute
//! tiers.
//!
//! **Issuance is registrar-only.** `commission_run` mints the sequence + the
//! commission token together and records them BEFORE the run executes;
//! `start_run` mints the launch nonce and records the run-start event BEFORE any
//! cell executes. Both use an injected generator (`&mut dyn FnMut() -> String`
//! — the crate's `idstore::mint_*` pattern), random/ULID in production,
//! deterministic in tests. Cell authors and runners never touch either path —
//! "issued by the registrar, not the producer" is structural.
//!
//! **Trust model (Pete-ruled `01KXEVSG33`; cooperative agents).** The launch
//! nonce makes assembly-after-start checkable (no artifact can serialize a
//! nonce that does not exist until `start_run` mints it). It does NOT, and
//! cannot, check execution-after-start — that the cell observations themselves
//! were physically produced after the start event — which is ASSERTED under
//! this trust model, never checked (R8-1). This module does not attempt to
//! close that gap; naming it here so no reader re-derives a stronger claim.

use serde::{Deserialize, Serialize};

use super::evidence::CommissioningTuple;
use super::ids::{
    AggregationVersion, ArtifactDigest, BoxId, CommissionToken, JournalOrdinal, LaneScope,
    LaunchNonce, ManifestDigest, RunId, RunKind, RunSequence,
};

/// An independently-authorized void ruling (N-1). A void must cite
/// outcome-independent evidence that **no canonical run artifact was produced**;
/// a void of an entry whose artifact DOES exist is invalid — but that check is
/// corpus-scope (A10 v4, C-3), because it needs the assembled corpus to know
/// whether the artifact exists. C-1 makes the ruling *representable* with the
/// evidence its validity requires, and records the authorizing seat (which must
/// be distinct from the registrar — an audit fact carried here, enforced by the
/// commissioning discipline, not by this type alone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoidRuling {
    /// The void authority's identity (instance name + commission id).
    pub authorized_by: String,
    /// Outcome-independent evidence that no canonical run artifact was produced.
    pub no_artifact_evidence: String,
}

/// The terminal for a STARTED entry that will produce no canonical artifact
/// (R6-6). Ruled by the same independent void-and-abort seat, never the runner,
/// never the registrar alone — **an abort cannot shadow evidence**, exactly like
/// a void: an abort ruling for an entry whose cited canonical artifact exists is
/// invalid (corpus-scope, A10 v4, C-3). Shares `VoidRuling`'s shape by design —
/// both require outcome-independent evidence of non-production — but is a
/// distinct type so the journal's terminal-state matrix keeps voided and
/// aborted separately auditable (R6-6 vs N-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbortRuling {
    /// The void-and-abort authority's identity (instance name + commission id).
    pub authorized_by: String,
    /// Outcome-independent evidence that no canonical run artifact was produced
    /// (a crash record, an infrastructure failure, a kill — evidence of the
    /// abort's cause, never of the run's would-be outcome).
    pub no_artifact_evidence: String,
}

/// The append-only terminal state every run entry must reach (N-1, R6-1, R6-6).
/// A non-terminal entry, a `Completed` entry whose cited artifact is missing
/// from the corpus, or a cardinality mismatch against this matrix, all fail
/// corpus terminal-completeness (A10 v4, fail-closed, C-3). Here the state is
/// simply *representable*, transitions are one-way, and the
/// started-entry-required / no-prior-start-required preconditions per variant
/// are enforced at append time (see [`AuthorityJournal::mark_terminal`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum TerminalState {
    /// Not yet terminal — the run is in flight or abandoned (started-open if a
    /// start event exists). Fails v4.
    NonTerminal,
    /// The canonical run artifact exists, cited by content digest (R6-1):
    /// publication and terminalization are ONE atomic journal append — there is
    /// no written-but-not-completed observation window. Valid only for an entry
    /// with a prior run-start event (R6-6).
    Completed { artifact_digest: ArtifactDigest },
    /// Cancelled before any cell executed. Valid ONLY if the entry has NO
    /// run-start event at any earlier ordinal (R6-6) — "before any cell
    /// executed" is a checkable ordinal fact, never an assertion.
    CancelledPreExecution { reason: String },
    /// Voided by an independently-authorized ruling. Void-cannot-shadow-evidence
    /// is corpus-scope (A10 v4, C-3).
    Voided(VoidRuling),
    /// Aborted: the terminal for a STARTED entry producing no canonical
    /// artifact (R6-6). Valid only for an entry with a prior run-start event.
    Aborted(AbortRuling),
}

impl TerminalState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, TerminalState::NonTerminal)
    }

    /// Whether this terminal kind REQUIRES a prior run-start event to be valid
    /// (Completed, Aborted) — both are the fate of a STARTED entry (R6-6).
    fn requires_prior_start(&self) -> bool {
        matches!(self, TerminalState::Completed { .. } | TerminalState::Aborted(_))
    }

    /// Whether this terminal kind is valid ONLY in the ABSENCE of a prior
    /// run-start event (CancelledPreExecution — R6-6).
    fn requires_no_prior_start(&self) -> bool {
        matches!(self, TerminalState::CancelledPreExecution { .. })
    }
}

/// A run-commissioning entry (R5-O3): the full immutable commissioning-context
/// tuple bound at commissioning. The sequence is the entry's shared ordinal
/// value (every event kind shares one domain), exposed typed as [`RunSequence`]
/// for A2 ordering. The run's terminal disposition is NOT a field here — it is a
/// separately-appended [`TerminalTransition`] event with its own ordinal (query
/// it via [`AuthorityJournal::terminal_state`]); this entry is immutable once
/// appended, so the journal is literally append-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEntry {
    pub run: RunId,
    pub lane_scope: LaneScope,
    pub box_id: BoxId,
    pub release_commit: String,
    pub manifest_digest: ManifestDigest,
    pub aggregation_version: AggregationVersion,
    /// `Exercise`-kind runs can never enter any window (A2(a)).
    pub run_kind: RunKind,
    pub sequence: RunSequence,
    pub commissioning_identity: String,
    pub token: CommissionToken,
}

/// The run-start / token-redemption event (R6-6, R7-2): registrar-sequenced
/// BEFORE any cell executes, binding the runner identity to the commissioned
/// run and minting the launch nonce — a fresh, single-use value first disclosed
/// HERE, unique across the journal, never derivable from the commission token,
/// the tuple, the run ID, or any earlier journal state. At most one per entry
/// (enforced at append). Every terminal completion or abort presupposes this
/// event exists; cancellation is valid only in its absence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStartEvent {
    pub run: RunId,
    pub runner_identity: String,
    pub launch_nonce: LaunchNonce,
    /// The run-start's **attested time** — descriptive only (ordering is by
    /// ordinal, never timestamp). This is the operand C-3's A10 stage-5
    /// run-liveness suppression reads (R6-1): a started-open evidence entry
    /// whose start attested time is more than Δ older than the anchoring
    /// snapshot's attested time suppresses the lane's window-dependent claims.
    /// `#[serde(default)]` so a journal serialized before this field existed
    /// still parses (an empty start time makes suppression fail-closed — a
    /// withheld completion cannot protect a tier). Set it via
    /// [`AuthorityJournal::start_run_at`].
    #[serde(default)]
    pub started_at: String,
}

/// A lane's evidence-box designation event (N-2, F-4). The transition state
/// machine that reduces these to `active` + optional `pending_replacement` — and
/// the promotion/premature-promotion validity — is A9's *replay*, which runs
/// inside A10 (C-3). C-1 carries the event's fields (the operands replay reads):
/// the enumerated outcome-independent policy basis, never observed pass/fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesignationKind {
    /// Initial designation of a lane's evidence box.
    Designate,
    /// A pending re-designation; its journal ordinal is the immutable
    /// effective-after boundary for the new box's accumulating window.
    PendingReplacement,
    /// The single atomic promotion event flipping the pending box to active.
    Promotion,
    /// Withdraw an outstanding pending replacement (before a second may be added).
    Withdrawal,
}

/// The TYPED, closed set of outcome-independent (re-)designation / withdrawal
/// basis KINDS (A9/R5-O5). This is the MACHINE-CHECKED basis: outcome-independence
/// is structural (there is no free-text field for an observed outcome to be
/// smuggled into, and no denylist to be synonym/spacing/homoglyph-evaded — the
/// lesson of [`super::attribution::ReasonCode`] and our redaction-denylist
/// experience). A closed enum: an unrecognized value fails to deserialize
/// (loud); an ABSENT kind (`None`, old journals) resolves to INVALID_EVIDENCE at
/// C-3's replay, never to a permissive default. **Honest residual (R8-1):** the
/// enum makes the basis unforgeable BY PHRASING, but it cannot verify the TRUTH
/// of the actor's chosen kind — an actor could select `GreaterRunnability` while
/// privately meaning "the box whose window looks best." That is audit's job and
/// the same cooperative-agents trust boundary the whole schema names; the typed
/// kind does NOT claim to close it — only to stop a dishonest basis from being
/// smuggled through free text, and to stop the code from claiming a completeness
/// a denylist lacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesignationBasisKind {
    /// The (pending/active) box was decommissioned.
    Decommissioned,
    /// The box became durably unreachable.
    DurablyUnreachable,
    /// The lane's applicable-cell runnability on the box was materially reduced,
    /// or the pending ruling's runnability basis was invalidated.
    RunnabilityReduced,
    /// A box achieving strictly greater applicable-cell runnability at
    /// representative shape (the "toward" basis; also the initial designation's
    /// maximal-runnability criterion).
    GreaterRunnability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignationEvent {
    pub kind: DesignationKind,
    pub lane: super::ids::Lane,
    pub box_id: BoxId,
    /// The MACHINE-CHECKED, typed outcome-independent basis (A9). `#[serde(default)]`
    /// (→ `None`) so a journal serialized before this field existed still parses;
    /// C-3's replay maps `None` (absent) → INVALID_EVIDENCE, never a permissive
    /// default. This replaces the free-text-basis word-denylist (RT-6): outcome
    /// independence is now structural, not a string check.
    #[serde(default)]
    pub basis_kind: Option<DesignationBasisKind>,
    /// Free-text DESCRIPTIVE companion for T4/audit readability ONLY — NOT the
    /// machine-checked basis (that is `basis_kind`). Nothing in the tier
    /// computation reads this for a validity decision.
    pub policy_basis: String,
    pub rationale: String,
    /// The designation authority's identity (instance name + commission id).
    pub authorized_by: String,
    /// Descriptive only — precedence is by journal ordinal, never timestamp.
    pub timestamp: String,
}

/// A context-activation event (R5-O7): fired whenever the battery manifest or
/// the aggregation version changes, so replay derives the version **effective
/// at any ordinal** — historical eligibility context is journal-derived, never
/// inferred from the runs under validation.
// ADJACENTLY tagged (tag + content), not internally tagged: an internally-tagged
// newtype variant wrapping a string-like value (`Manifest(ManifestDigest)`) is a
// representation serde_json cannot serialize (it compiles but errs at runtime —
// "cannot serialize tagged newtype variant … containing a string"), and C-3's
// corpus MUST round-trip through JSON for the fresh-reader byte-match. Adjacent
// tagging (`{"activates":"manifest","context":"sha256:…"}`) round-trips cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "activates", content = "context", rename_all = "kebab-case")]
pub enum ContextActivationKind {
    Manifest(ManifestDigest),
    AggregationVersion(AggregationVersion),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextActivationEvent {
    // Named `activation` (not `kind`) so it does not collide with `JournalBody`'s
    // internal `#[serde(tag = "kind")]` discriminator when this variant's struct
    // fields are merged — a collision that makes a journal carrying a
    // context-activation event fail to round-trip through JSON, which C-3's
    // fresh-reader byte-match requires.
    pub activation: ContextActivationKind,
}

/// A publication-snapshot event (R6-5): registrar-appended attestation binding,
/// at the snapshot's own ordinal, the artifact-index head digest the registrar
/// observes plus an attested observation timestamp. Monotonically increasing by
/// journal construction (ordinals only ever grow); every tier computation
/// anchors at exactly one (A10 v7), and every published tier row cites it — the
/// head digest proves prefix integrity, but only the snapshot's ordinal + the
/// no-rollback rule (C-3) prove currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationSnapshotEvent {
    pub artifact_index_head_digest: ArtifactDigest,
    /// Attested observation time — descriptive of WHEN the registrar observed
    /// the head, never a precedence input (ordering is by ordinal).
    pub observed_at: String,
}

/// A run-terminal transition event (N-1, R6-1, R6-6) — the sixth event kind,
/// appended to the journal with its OWN ordinal M, distinct from and strictly
/// greater than the run's commissioning ordinal N (and greater than any
/// run-start ordinal). This is what makes "each terminal transition is itself a
/// journal event with its own ordinal" (R9) literally true: the disposition is
/// an appended event, NOT a mutable field on the commissioning entry. C-3's
/// promotion/withdrawal arithmetic keys on this terminal ordinal (R9: "derived
/// promotion keys on the terminal event's ordinal — A9/R5-O5"; "withdrawal
/// validity is pure ordinal arithmetic over run-start and terminal events"). At
/// most one per run entry (enforced at append and at parse); the terminal state
/// itself is one-way. The R6-6 start-precondition per terminal kind
/// (Completed/Aborted require a prior run-start; CancelledPreExecution requires
/// its absence) is enforced on this event, not on any field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTransition {
    pub run: RunId,
    /// The terminal reached. Never [`TerminalState::NonTerminal`] — a terminal
    /// transition event carrying a non-terminal state is malformed (rejected at
    /// append and flagged by [`AuthorityJournal::integrity_check`]).
    pub terminal: TerminalState,
}

/// One journal entry, tagged by kind. Every entry carries its ordinal (its
/// position in the one ordered domain) — all six event kinds share it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub ordinal: JournalOrdinal,
    pub body: JournalBody,
}

// Internally tagged on `event` (not `kind`): the `Designation` and
// `ContextActivation` variants each wrap a struct that itself has a `kind`-named
// field; an internal tag of `kind` would collide with those on serialization and
// break the round-trip C-3's fresh-reader byte-match requires. `event` is unique
// across every variant's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum JournalBody {
    Run(RunEntry),
    Start(RunStartEvent),
    Terminal(TerminalTransition),
    Designation(DesignationEvent),
    ContextActivation(ContextActivationEvent),
    PublicationSnapshot(PublicationSnapshotEvent),
}

/// The one append-only authority journal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityJournal {
    entries: Vec<JournalEntry>,
}

impl AuthorityJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next ordinal value = the shared strictly-increasing counter. One past
    /// the highest ordinal in the journal (ordinals are dense from 1 here, but
    /// the property that matters is strictly-increasing + unique, which
    /// [`Self::integrity_check`] verifies for any parsed journal).
    fn next_value(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.ordinal.get())
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Commission a run: mint the next sequence + a fresh commission token,
    /// append a `NonTerminal` run entry BEFORE execution binding the full
    /// commissioning-context tuple (R5-O3), and return the [`CommissioningTuple`]
    /// the run-start event and the artifact header must reproduce. Rejects a
    /// duplicate run id loudly (a replayed/duplicate entry is never honored, N-1).
    #[allow(clippy::too_many_arguments)]
    pub fn commission_run(
        &mut self,
        run: RunId,
        lane_scope: LaneScope,
        box_id: BoxId,
        release_commit: impl Into<String>,
        manifest_digest: ManifestDigest,
        aggregation_version: AggregationVersion,
        run_kind: RunKind,
        commissioning_identity: impl Into<String>,
        mint_token: &mut dyn FnMut() -> String,
    ) -> Result<CommissioningTuple, String> {
        if self.entries.iter().any(|e| matches!(&e.body, JournalBody::Run(r) if r.run == run)) {
            return Err(format!(
                "authority journal: duplicate run id {:?} — rejected (a replayed/duplicate entry is never honored, N-1)",
                run.0
            ));
        }
        let value = self.next_value();
        let ordinal = JournalOrdinal::new(value);
        let sequence = RunSequence::new(value);
        let token = CommissionToken::new(mint_token());
        // A token must be non-empty and unique across the journal — the registrar
        // is the sole minter; a colliding token is a generator failure, loud.
        if token.as_str().trim().is_empty() {
            return Err("authority journal: registrar minted an empty commission token".into());
        }
        if self.token_exists(&token) {
            return Err(format!(
                "authority journal: commission token {:?} collides with an existing entry — rejected",
                token.as_str()
            ));
        }
        let commissioning_identity = commissioning_identity.into();
        let release_commit = release_commit.into();
        self.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::Run(RunEntry {
                run: run.clone(),
                lane_scope: lane_scope.clone(),
                box_id: box_id.clone(),
                release_commit: release_commit.clone(),
                manifest_digest: manifest_digest.clone(),
                aggregation_version: aggregation_version.clone(),
                run_kind,
                sequence,
                commissioning_identity: commissioning_identity.clone(),
                token: token.clone(),
            }),
        });
        Ok(CommissioningTuple {
            run,
            lane_scope,
            box_id,
            release_commit,
            manifest_digest,
            aggregation_version,
            run_kind,
            sequence,
            commissioning_identity,
            token,
        })
    }

    /// Redeem the commission: append the run-start event BEFORE any cell
    /// executes, minting the fresh single-use launch nonce (R7-2) and binding
    /// the runner identity. At most one start event per entry. For
    /// `RunKind::Evidence` runs, the runner identity MUST differ from the
    /// entry's commissioning identity (R6-6) — equal is structurally invalid,
    /// so the party able to file a cancellation is structurally not the party
    /// that could have executed.
    pub fn start_run(
        &mut self,
        run: &RunId,
        runner_identity: impl Into<String>,
        mint_nonce: &mut dyn FnMut() -> String,
    ) -> Result<LaunchNonce, String> {
        // Delegates with an empty attested time — the run-liveness deadline
        // (R6-1) then keys fail-closed for this entry. Callers that record a
        // start time use [`Self::start_run_at`].
        self.start_run_at(run, runner_identity, "", mint_nonce)
    }

    /// Like [`Self::start_run`] but records the run-start's **attested time**
    /// (R6-1) — the operand A10 stage-5 run-liveness suppression reads. Time is
    /// descriptive only; ordering is always by ordinal.
    pub fn start_run_at(
        &mut self,
        run: &RunId,
        runner_identity: impl Into<String>,
        started_at: impl Into<String>,
        mint_nonce: &mut dyn FnMut() -> String,
    ) -> Result<LaunchNonce, String> {
        let entry = self
            .run_entry(run)
            .ok_or_else(|| format!("authority journal: no run entry for {:?}", run.0))?
            .clone();
        if self.has_start_event(run) {
            return Err(format!(
                "authority journal: run {:?} already has a run-start event — at most one per entry",
                run.0
            ));
        }
        let runner_identity = runner_identity.into();
        if entry.run_kind == RunKind::Evidence && runner_identity == entry.commissioning_identity {
            return Err(format!(
                "authority journal: run {:?} — commissioning identity equals runner identity on an evidence-kind run — structurally invalid (R6-6)",
                run.0
            ));
        }
        let nonce = LaunchNonce::new(mint_nonce());
        if nonce.as_str().trim().is_empty() {
            return Err("authority journal: registrar minted an empty launch nonce".into());
        }
        if self.nonce_exists(&nonce) {
            return Err(format!(
                "authority journal: launch nonce {:?} collides with an existing entry — rejected (R7-2 uniqueness)",
                nonce.as_str()
            ));
        }
        let ordinal = JournalOrdinal::new(self.next_value());
        self.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::Start(RunStartEvent {
                run: run.clone(),
                runner_identity,
                launch_nonce: nonce.clone(),
                started_at: started_at.into(),
            }),
        });
        Ok(nonce)
    }

    /// Append a designation event, minting the next ordinal. (Replay + transition
    /// validity is A9/A10, C-3.)
    pub fn designate(&mut self, event: DesignationEvent) -> JournalOrdinal {
        let ordinal = JournalOrdinal::new(self.next_value());
        self.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::Designation(event),
        });
        ordinal
    }

    /// Append a context-activation event (R5-O7), minting the next ordinal.
    pub fn activate_context(&mut self, kind: ContextActivationKind) -> JournalOrdinal {
        let ordinal = JournalOrdinal::new(self.next_value());
        self.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::ContextActivation(ContextActivationEvent { activation: kind }),
        });
        ordinal
    }

    /// The manifest digest effective at `ordinal` — the latest manifest
    /// activation at or before it, derived by replay (R5-O7). `None` if no
    /// manifest activation exists at or before `ordinal`.
    pub fn manifest_digest_effective_at(&self, ordinal: JournalOrdinal) -> Option<&ManifestDigest> {
        self.entries
            .iter()
            .filter(|e| e.ordinal.get() <= ordinal.get())
            .filter_map(|e| match &e.body {
                JournalBody::ContextActivation(ContextActivationEvent {
                    activation: ContextActivationKind::Manifest(d),
                }) => Some((e.ordinal.get(), d)),
                _ => None,
            })
            .max_by_key(|(ord, _)| *ord)
            .map(|(_, d)| d)
    }

    /// The aggregation version effective at `ordinal` — the latest aggregation
    /// activation at or before it (R5-O7). `None` if none exists yet.
    pub fn aggregation_version_effective_at(
        &self,
        ordinal: JournalOrdinal,
    ) -> Option<&AggregationVersion> {
        self.entries
            .iter()
            .filter(|e| e.ordinal.get() <= ordinal.get())
            .filter_map(|e| match &e.body {
                JournalBody::ContextActivation(ContextActivationEvent {
                    activation: ContextActivationKind::AggregationVersion(v),
                }) => Some((e.ordinal.get(), v)),
                _ => None,
            })
            .max_by_key(|(ord, _)| *ord)
            .map(|(_, v)| v)
    }

    /// Append a publication-snapshot event (R6-5), minting the next ordinal.
    /// Monotonically increasing by journal construction — ordinals only grow.
    pub fn publish_snapshot(
        &mut self,
        artifact_index_head_digest: ArtifactDigest,
        observed_at: impl Into<String>,
    ) -> JournalOrdinal {
        let ordinal = JournalOrdinal::new(self.next_value());
        self.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::PublicationSnapshot(PublicationSnapshotEvent {
                artifact_index_head_digest,
                observed_at: observed_at.into(),
            }),
        });
        ordinal
    }

    /// The most recent publication-snapshot event (every tier computation
    /// anchors at exactly one, A10 v7).
    pub fn latest_snapshot(&self) -> Option<(JournalOrdinal, &PublicationSnapshotEvent)> {
        self.entries.iter().rev().find_map(|e| match &e.body {
            JournalBody::PublicationSnapshot(ev) => Some((e.ordinal, ev)),
            _ => None,
        })
    }

    /// Drive a run entry to a terminal state by APPENDING a [`TerminalTransition`]
    /// event with its own fresh ordinal (M > the commissioning ordinal N, and > any
    /// run-start ordinal) — never a field mutation, so the journal is literally
    /// append-only and C-3 has a distinct terminal ordinal to key promotion/
    /// withdrawal arithmetic on (R9). One-way: a second terminal transition for an
    /// already-terminal run is rejected (dispositions are recorded once;
    /// corrections would be new events, not overwrites). Enforces the R6-6
    /// start-event preconditions per terminal kind: `Completed`/`Aborted` require a
    /// prior run-start event (they are the fate of a STARTED entry);
    /// `CancelledPreExecution` requires the ABSENCE of one ("before any cell
    /// executed" — a checkable ordinal fact, never asserted). A `NonTerminal`
    /// argument is rejected — this path only records real terminals.
    pub fn mark_terminal(&mut self, run: &RunId, terminal: TerminalState) -> Result<(), String> {
        if !terminal.is_terminal() {
            return Err(format!(
                "authority journal: run {:?} — NonTerminal is not a terminal disposition and cannot be recorded as one",
                run.0
            ));
        }
        if self.run_entry(run).is_none() {
            return Err(format!("authority journal: no run entry for {:?}", run.0));
        }
        if self.terminal_event(run).is_some() {
            return Err(format!(
                "authority journal: run {:?} is already terminal ({:?}) — no overwrite (corrections are new events, N-1)",
                run.0,
                self.terminal_state(run)
            ));
        }
        let has_start = self.has_start_event(run);
        if terminal.requires_prior_start() && !has_start {
            return Err(format!(
                "authority journal: run {:?} — {:?} requires a prior run-start event (R6-6: completion/abort is the fate of a STARTED entry)",
                run.0, terminal
            ));
        }
        if terminal.requires_no_prior_start() && has_start {
            return Err(format!(
                "authority journal: run {:?} — cancellation after a run-start event is structurally invalid (R6-6)",
                run.0
            ));
        }
        let ordinal = JournalOrdinal::new(self.next_value());
        self.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::Terminal(TerminalTransition {
                run: run.clone(),
                terminal,
            }),
        });
        Ok(())
    }

    /// The terminal transition event for `run`, if one has been appended (at most
    /// one can ever exist). Terminal state is journal-derived from this event,
    /// never a field on the commissioning entry.
    pub fn terminal_event(&self, run: &RunId) -> Option<&TerminalTransition> {
        self.entries.iter().find_map(|e| match &e.body {
            JournalBody::Terminal(t) if &t.run == run => Some(t),
            _ => None,
        })
    }

    /// The terminal state of `run`: the state of its appended terminal event, or
    /// [`TerminalState::NonTerminal`] if none has landed yet (a run in flight or
    /// abandoned — whether that is a corpus-completeness failure is C-3's A10 v4,
    /// not C-1's).
    pub fn terminal_state(&self, run: &RunId) -> TerminalState {
        self.terminal_event(run)
            .map(|t| t.terminal.clone())
            .unwrap_or(TerminalState::NonTerminal)
    }

    fn token_exists(&self, token: &CommissionToken) -> bool {
        self.entries.iter().any(|e| match &e.body {
            JournalBody::Run(r) => &r.token == token,
            _ => false,
        })
    }

    fn nonce_exists(&self, nonce: &LaunchNonce) -> bool {
        self.entries.iter().any(|e| match &e.body {
            JournalBody::Start(s) => &s.launch_nonce == nonce,
            _ => false,
        })
    }

    /// Whether `run` has a run-start event anywhere in the journal (at most one
    /// can ever be appended, so existence alone answers "started").
    pub fn has_start_event(&self, run: &RunId) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(&e.body, JournalBody::Start(s) if &s.run == run))
    }

    /// The run-start event for `run`, if one has been appended.
    pub fn start_event(&self, run: &RunId) -> Option<&RunStartEvent> {
        self.entries.iter().find_map(|e| match &e.body {
            JournalBody::Start(s) if &s.run == run => Some(s),
            _ => None,
        })
    }

    /// The ordinal of `run`'s run-start event, if one has been appended — the
    /// operand a started-terminal's ordinal must exceed (R6-6/R9).
    pub fn start_ordinal(&self, run: &RunId) -> Option<u64> {
        self.entries.iter().find_map(|e| match &e.body {
            JournalBody::Start(s) if &s.run == run => Some(e.ordinal.get()),
            _ => None,
        })
    }

    /// Find a run entry by run id.
    pub fn run_entry(&self, run: &RunId) -> Option<&RunEntry> {
        self.entries.iter().find_map(|e| match &e.body {
            JournalBody::Run(r) if &r.run == run => Some(r),
            _ => None,
        })
    }

    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    pub fn run_entries(&self) -> impl Iterator<Item = &RunEntry> {
        self.entries.iter().filter_map(|e| match &e.body {
            JournalBody::Run(r) => Some(r),
            _ => None,
        })
    }

    /// The journal's own well-formedness — checkable on ANY parsed journal, so a
    /// loaded/forged journal is rejected the same as one built through the API.
    /// This is the journal-integrity operand A10 v1 (C-3) consumes over the
    /// corpus's journal. Checks:
    ///   - ordinals strictly increasing in entry order AND globally unique
    ///   - run ids unique (no duplicate/replayed run entries)
    ///   - commission tokens unique and non-empty
    ///   - run sequence equals its ordinal value (the one-domain invariant)
    ///   - at most one run-start event per run id (R6-6)
    ///   - launch nonces unique and non-empty (R7-2)
    ///   - a run-start event's runner identity differs from its entry's
    ///     commissioning identity, for evidence-kind runs (R6-6)
    ///   - every run-start event references an existing run entry and lands at an
    ///     ordinal strictly greater than that entry's commissioning ordinal N
    ///     (start redeems a prior commission — the API always commissions first)
    ///   - at most one terminal transition event per run id (one-way disposition)
    ///   - every terminal transition references an existing run entry, carries a
    ///     genuinely-terminal state, and lands at an ordinal strictly greater than
    ///     its commissioning ordinal N (and than its run-start ordinal) — this is
    ///     the operand C-3's promotion/withdrawal arithmetic reads (R9)
    ///   - `Completed`/`Aborted` terminals only on runs with a run-start event;
    ///     `CancelledPreExecution` only on runs WITHOUT one (R6-6)
    pub fn integrity_check(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let mut seen_ord = std::collections::BTreeSet::new();
        let mut seen_run = std::collections::BTreeSet::new();
        let mut seen_tok = std::collections::BTreeSet::new();
        let mut seen_start_run = std::collections::BTreeSet::new();
        let mut seen_terminal_run = std::collections::BTreeSet::new();
        let mut seen_nonce = std::collections::BTreeSet::new();
        let mut prev: Option<u64> = None;
        for e in &self.entries {
            let ord = e.ordinal.get();
            if let Some(p) = prev {
                if ord <= p {
                    errs.push(format!(
                        "ordinals not strictly increasing: {ord} follows {p}"
                    ));
                }
            }
            prev = Some(ord);
            if !seen_ord.insert(ord) {
                errs.push(format!("duplicate ordinal {ord}"));
            }
            match &e.body {
                JournalBody::Run(r) => {
                    if !seen_run.insert(r.run.0.clone()) {
                        errs.push(format!("duplicate run id {:?}", r.run.0));
                    }
                    if r.token.as_str().trim().is_empty() {
                        errs.push(format!("empty commission token on run {:?}", r.run.0));
                    } else if !seen_tok.insert(r.token.as_str().to_string()) {
                        errs.push(format!("duplicate commission token {:?}", r.token.as_str()));
                    }
                    if r.sequence.get() != ord {
                        errs.push(format!(
                            "run {:?} sequence {} != ordinal {} (one-domain invariant violated)",
                            r.run.0,
                            r.sequence.get(),
                            ord
                        ));
                    }
                }
                JournalBody::Start(s) => {
                    if !seen_start_run.insert(s.run.0.clone()) {
                        errs.push(format!(
                            "run {:?} has more than one run-start event (R6-6: at most one per entry)",
                            s.run.0
                        ));
                    }
                    if s.launch_nonce.as_str().trim().is_empty() {
                        errs.push(format!("empty launch nonce on run-start for {:?}", s.run.0));
                    } else if !seen_nonce.insert(s.launch_nonce.as_str().to_string()) {
                        errs.push(format!(
                            "duplicate launch nonce {:?} (R7-2: unique across the journal)",
                            s.launch_nonce.as_str()
                        ));
                    }
                    match self.run_entry(&s.run) {
                        Some(entry) => {
                            if entry.run_kind == RunKind::Evidence
                                && s.runner_identity == entry.commissioning_identity
                            {
                                errs.push(format!(
                                    "run {:?}: commissioning identity equals runner identity on an evidence-kind run (R6-6)",
                                    s.run.0
                                ));
                            }
                            // The run-start ordinal must land strictly AFTER its
                            // commissioning ordinal N — the registrar commissions
                            // BEFORE it redeems (the API always appends the Run
                            // entry before the Start event). Symmetric to the
                            // Terminal arm's ordinal check; catches a forged
                            // journal the live API could never produce.
                            if ord <= entry.sequence.get() {
                                errs.push(format!(
                                    "run {:?}: run-start ordinal {} not strictly after its commissioning ordinal {} (start redeems a prior commission)",
                                    s.run.0, ord, entry.sequence.get()
                                ));
                            }
                        }
                        None => errs.push(format!(
                            "run-start event for {:?} has no matching run-commissioning entry",
                            s.run.0
                        )),
                    }
                }
                JournalBody::Terminal(t) => {
                    if !seen_terminal_run.insert(t.run.0.clone()) {
                        errs.push(format!(
                            "run {:?} has more than one terminal transition event (a disposition is recorded once, N-1)",
                            t.run.0
                        ));
                    }
                    if !t.terminal.is_terminal() {
                        errs.push(format!(
                            "run {:?}: terminal transition event carries a non-terminal state (malformed)",
                            t.run.0
                        ));
                    }
                    // Terminal ordinal must exist AFTER the commissioning ordinal
                    // (and, for started terminals, after the run-start ordinal) —
                    // the distinct M > N C-3 keys promotion/withdrawal on (R9).
                    match self.run_entry(&t.run) {
                        Some(entry) => {
                            if ord <= entry.sequence.get() {
                                errs.push(format!(
                                    "run {:?}: terminal ordinal {} not strictly after its commissioning ordinal {} (R9: terminal is a later event)",
                                    t.run.0, ord, entry.sequence.get()
                                ));
                            }
                        }
                        None => errs.push(format!(
                            "terminal transition for {:?} has no matching run-commissioning entry",
                            t.run.0
                        )),
                    }
                    let started = self.has_start_event(&t.run);
                    if let Some(start_ord) = self.start_ordinal(&t.run) {
                        if ord <= start_ord {
                            errs.push(format!(
                                "run {:?}: terminal ordinal {} not strictly after its run-start ordinal {} (R6-6/R9)",
                                t.run.0, ord, start_ord
                            ));
                        }
                    }
                    if t.terminal.requires_prior_start() && !started {
                        errs.push(format!(
                            "run {:?}: {:?} requires a prior run-start event (R6-6)",
                            t.run.0, t.terminal
                        ));
                    }
                    if t.terminal.requires_no_prior_start() && started {
                        errs.push(format!(
                            "run {:?}: cancellation after a run-start event is structurally invalid (R6-6)",
                            t.run.0
                        ));
                    }
                }
                JournalBody::Designation(_)
                | JournalBody::ContextActivation(_)
                | JournalBody::PublicationSnapshot(_) => {}
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::ids::Lane;

    fn seq_gen(prefix: &'static str) -> impl FnMut() -> String {
        let mut n = 0u64;
        move || {
            n += 1;
            format!("{prefix}-{n}")
        }
    }

    fn commission_evidence(journal: &mut AuthorityJournal, run: &str) -> CommissioningTuple {
        // Token uniqueness is journal-wide (N-1), so the mint must be
        // per-run-unique across the whole test module — embed `run` itself
        // rather than resetting a counter per call (which would collide
        // whenever two runs are commissioned into the same journal).
        journal
            .commission_run(
                RunId(run.into()),
                LaneScope::one(Lane::Pi),
                BoxId("lima".into()),
                "commit-abc",
                ManifestDigest("sha256:deadbeef".into()),
                AggregationVersion("agg-v1".into()),
                RunKind::Evidence,
                "registrar-1 (uq5xx83a)",
                &mut || format!("tok-{run}"),
            )
            .unwrap()
    }

    #[test]
    fn commissioning_identity_equal_to_runner_identity_is_rejected_for_evidence_runs() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r1");
        let err = j
            .start_run(&tuple.run, "registrar-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap_err();
        assert!(err.contains("structurally invalid"), "{err}");
    }

    #[test]
    fn start_run_mints_a_fresh_unique_nonce_and_at_most_one_per_entry() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r2");
        let nonce = j
            .start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        assert!(!nonce.as_str().is_empty());
        let err = j
            .start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap_err();
        assert!(err.contains("at most one"), "{err}");
    }

    #[test]
    fn completion_without_start_is_structurally_invalid() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r3");
        let err = j
            .mark_terminal(
                &tuple.run,
                TerminalState::Completed {
                    artifact_digest: ArtifactDigest("sha256:cafe".into()),
                },
            )
            .unwrap_err();
        assert!(err.contains("prior run-start"), "{err}");
    }

    #[test]
    fn cancellation_after_start_is_structurally_invalid() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r4");
        j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        let err = j
            .mark_terminal(
                &tuple.run,
                TerminalState::CancelledPreExecution {
                    reason: "changed mind".into(),
                },
            )
            .unwrap_err();
        assert!(err.contains("cancellation after a run-start"), "{err}");
    }

    #[test]
    fn cancellation_before_start_is_valid() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r5");
        j.mark_terminal(
            &tuple.run,
            TerminalState::CancelledPreExecution {
                reason: "box unavailable".into(),
            },
        )
        .unwrap();
        assert!(j.terminal_state(&tuple.run).is_terminal());
    }

    #[test]
    fn completion_after_start_is_valid_and_cites_digest() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r6");
        j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        j.mark_terminal(
            &tuple.run,
            TerminalState::Completed {
                artifact_digest: ArtifactDigest("sha256:cafe".into()),
            },
        )
        .unwrap();
        match j.terminal_state(&tuple.run) {
            TerminalState::Completed { artifact_digest } => {
                assert_eq!(artifact_digest.0, "sha256:cafe");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn abort_after_start_is_valid() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r7");
        j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        j.mark_terminal(
            &tuple.run,
            TerminalState::Aborted(AbortRuling {
                authorized_by: "void-seat-1 (uq5xx83a)".into(),
                no_artifact_evidence: "harness crash record ref-123".into(),
            }),
        )
        .unwrap();
        assert!(matches!(
            j.terminal_state(&tuple.run),
            TerminalState::Aborted(_)
        ));
    }

    #[test]
    fn duplicate_nonce_is_rejected_even_across_two_runs() {
        let mut j = AuthorityJournal::new();
        let t1 = commission_evidence(&mut j, "r8");
        let t2 = commission_evidence(&mut j, "r9");
        j.start_run(&t1.run, "runner-1 (uq5xx83a)", &mut || "fixed-nonce".to_string())
            .unwrap();
        let err = j
            .start_run(&t2.run, "runner-2 (uq5xx83a)", &mut || "fixed-nonce".to_string())
            .unwrap_err();
        assert!(err.contains("collides"), "{err}");
    }

    #[test]
    fn context_activation_replay_derives_effective_versions_at_any_ordinal() {
        let mut j = AuthorityJournal::new();
        let before = commission_evidence(&mut j, "before-activation");
        let ord1 = j.activate_context(ContextActivationKind::Manifest(ManifestDigest(
            "sha256:v1".into(),
        )));
        let ord2 = j.activate_context(ContextActivationKind::Manifest(ManifestDigest(
            "sha256:v2".into(),
        )));
        let before_ordinal = JournalOrdinal::new(before.sequence.get());
        assert_eq!(j.manifest_digest_effective_at(before_ordinal), None);
        assert_eq!(
            j.manifest_digest_effective_at(ord1).unwrap().0,
            "sha256:v1"
        );
        assert_eq!(
            j.manifest_digest_effective_at(ord2).unwrap().0,
            "sha256:v2"
        );
    }

    #[test]
    fn publication_snapshot_is_monotonic_and_latest_wins() {
        let mut j = AuthorityJournal::new();
        let o1 = j.publish_snapshot(ArtifactDigest("sha256:idx1".into()), "2026-07-13T00:00:00Z");
        let o2 = j.publish_snapshot(ArtifactDigest("sha256:idx2".into()), "2026-07-13T01:00:00Z");
        assert!(o2.get() > o1.get());
        let (latest_ord, latest) = j.latest_snapshot().unwrap();
        assert_eq!(latest_ord, o2);
        assert_eq!(latest.artifact_index_head_digest.0, "sha256:idx2");
    }

    #[test]
    fn integrity_check_catches_a_forged_journal_with_completion_and_no_start() {
        // Build a journal, then hand-forge a Completed TERMINAL EVENT past its
        // normal API (simulating a loaded/parsed journal that never went through
        // mark_terminal's precondition check) — a Completed for a run with NO
        // run-start event — to prove integrity_check catches it independently of
        // the live API. The terminal is a separately-appended event now, so the
        // forge appends one rather than mutating a field.
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r10");
        let ordinal = JournalOrdinal::new(j.next_value());
        j.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::Terminal(TerminalTransition {
                run: tuple.run.clone(),
                terminal: TerminalState::Completed {
                    artifact_digest: ArtifactDigest("sha256:forged".into()),
                },
            }),
        });
        let errs = j.integrity_check().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("prior run-start")), "{errs:?}");
    }

    #[test]
    fn integrity_check_is_clean_on_a_well_formed_journal() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "r11");
        j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        j.mark_terminal(
            &tuple.run,
            TerminalState::Completed {
                artifact_digest: ArtifactDigest("sha256:cafe".into()),
            },
        )
        .unwrap();
        j.integrity_check().unwrap();
    }

    // ---- F1: the terminal transition is its own appended, ordinal-bearing event.

    /// The load-bearing F1 property: marking a run terminal APPENDS a
    /// [`TerminalTransition`] event whose ordinal M is strictly greater than the
    /// commissioning ordinal N and the run-start ordinal — the distinct operand
    /// C-3 keys promotion/withdrawal arithmetic on (R9). It is NOT a mutation of
    /// the commissioning entry.
    #[test]
    fn terminal_transition_is_an_appended_event_with_its_own_ordinal() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "rT");
        let commissioning_ord = tuple.sequence.get();
        let start_ord = {
            j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
                .unwrap();
            j.start_ordinal(&tuple.run).unwrap()
        };
        let entries_before = j.entries().len();
        j.mark_terminal(
            &tuple.run,
            TerminalState::Completed {
                artifact_digest: ArtifactDigest("sha256:cafe".into()),
            },
        )
        .unwrap();
        // A new entry was appended (not a field mutated in place).
        assert_eq!(j.entries().len(), entries_before + 1);
        // Its terminal state is queryable off the appended event.
        assert!(matches!(
            j.terminal_state(&tuple.run),
            TerminalState::Completed { .. }
        ));
        // And it carries its OWN ordinal M > N and > the run-start ordinal.
        let terminal_ord = j
            .entries()
            .iter()
            .find_map(|e| match &e.body {
                JournalBody::Terminal(t) if t.run == tuple.run => Some(e.ordinal.get()),
                _ => None,
            })
            .expect("a terminal event was appended");
        assert!(
            terminal_ord > commissioning_ord,
            "terminal ordinal {terminal_ord} must exceed commissioning ordinal {commissioning_ord}"
        );
        assert!(
            terminal_ord > start_ord,
            "terminal ordinal {terminal_ord} must exceed run-start ordinal {start_ord}"
        );
        j.integrity_check().unwrap();
    }

    /// A disposition is recorded once: a second terminal transition for an
    /// already-terminal run is rejected (corrections would be new events, not
    /// overwrites).
    #[test]
    fn a_second_terminal_transition_is_rejected() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "rT2");
        j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        j.mark_terminal(
            &tuple.run,
            TerminalState::Completed {
                artifact_digest: ArtifactDigest("sha256:cafe".into()),
            },
        )
        .unwrap();
        let err = j
            .mark_terminal(
                &tuple.run,
                TerminalState::Aborted(AbortRuling {
                    authorized_by: "void-seat-1 (uq5xx83a)".into(),
                    no_artifact_evidence: "late abort".into(),
                }),
            )
            .unwrap_err();
        assert!(err.contains("already terminal"), "{err}");
    }

    /// `mark_terminal` refuses a `NonTerminal` argument — this path records real
    /// terminals only.
    #[test]
    fn mark_terminal_rejects_a_non_terminal_argument() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "rT3");
        let err = j
            .mark_terminal(&tuple.run, TerminalState::NonTerminal)
            .unwrap_err();
        assert!(err.contains("NonTerminal"), "{err}");
    }

    /// integrity_check catches a hand-forged journal with two terminal events
    /// for one run (a disposition recorded twice).
    #[test]
    fn integrity_check_catches_a_duplicate_terminal_event() {
        let mut j = AuthorityJournal::new();
        let tuple = commission_evidence(&mut j, "rT4");
        j.start_run(&tuple.run, "runner-1 (uq5xx83a)", &mut seq_gen("nonce"))
            .unwrap();
        j.mark_terminal(
            &tuple.run,
            TerminalState::Completed {
                artifact_digest: ArtifactDigest("sha256:cafe".into()),
            },
        )
        .unwrap();
        // Forge a second terminal event past the API precondition.
        let ordinal = JournalOrdinal::new(j.next_value());
        j.entries.push(JournalEntry {
            ordinal,
            body: JournalBody::Terminal(TerminalTransition {
                run: tuple.run.clone(),
                terminal: TerminalState::Aborted(AbortRuling {
                    authorized_by: "void-seat-1 (uq5xx83a)".into(),
                    no_artifact_evidence: "forged second disposition".into(),
                }),
            }),
        });
        let errs = j.integrity_check().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("more than one terminal transition")),
            "{errs:?}"
        );
    }

    /// integrity_check catches a terminal event whose ordinal is NOT after its
    /// commissioning ordinal (a forged out-of-order disposition).
    #[test]
    fn integrity_check_catches_a_terminal_event_out_of_order() {
        // Hand-build a journal where the terminal event's ordinal precedes the
        // commissioning ordinal — impossible through the API, caught at parse.
        let mut j = AuthorityJournal::new();
        j.entries.push(JournalEntry {
            ordinal: JournalOrdinal::new(1),
            body: JournalBody::Terminal(TerminalTransition {
                run: RunId("rOOO".into()),
                terminal: TerminalState::CancelledPreExecution {
                    reason: "forged early terminal".into(),
                },
            }),
        });
        j.entries.push(JournalEntry {
            ordinal: JournalOrdinal::new(2),
            body: JournalBody::Run(RunEntry {
                run: RunId("rOOO".into()),
                lane_scope: LaneScope::one(Lane::Pi),
                box_id: BoxId("lima".into()),
                release_commit: "commit-abc".into(),
                manifest_digest: ManifestDigest("sha256:deadbeef".into()),
                aggregation_version: AggregationVersion("agg-v1".into()),
                run_kind: RunKind::Evidence,
                sequence: RunSequence::new(2),
                commissioning_identity: "registrar-1 (uq5xx83a)".into(),
                token: CommissionToken::new("tok-ooo"),
            }),
        });
        let errs = j.integrity_check().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("terminal ordinal") && e.contains("not strictly after its commissioning ordinal")),
            "{errs:?}"
        );
    }

    /// integrity_check catches a forged run-start whose ordinal PRECEDES its
    /// commissioning ordinal (Start at ordinal 1, Run/commission at ordinal 2 for
    /// the same run id) — impossible through the API (commission_run always
    /// appends the Run entry before start_run appends the Start event), caught at
    /// parse. Symmetric to the terminal-ordinal forge above.
    #[test]
    fn integrity_check_catches_a_run_start_before_its_commission() {
        let mut j = AuthorityJournal::new();
        j.entries.push(JournalEntry {
            ordinal: JournalOrdinal::new(1),
            body: JournalBody::Start(RunStartEvent {
                run: RunId("rSB".into()),
                runner_identity: "runner-1 (uq5xx83a)".into(),
                launch_nonce: LaunchNonce::new("nonce-sb"),
                started_at: String::new(),
            }),
        });
        j.entries.push(JournalEntry {
            ordinal: JournalOrdinal::new(2),
            body: JournalBody::Run(RunEntry {
                run: RunId("rSB".into()),
                lane_scope: LaneScope::one(Lane::Pi),
                box_id: BoxId("lima".into()),
                release_commit: "commit-abc".into(),
                manifest_digest: ManifestDigest("sha256:deadbeef".into()),
                aggregation_version: AggregationVersion("agg-v1".into()),
                run_kind: RunKind::Evidence,
                sequence: RunSequence::new(2),
                commissioning_identity: "registrar-1 (uq5xx83a)".into(),
                token: CommissionToken::new("tok-sb"),
            }),
        });
        let errs = j.integrity_check().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("run-start ordinal") && e.contains("not strictly after its commissioning ordinal")),
            "{errs:?}"
        );
    }
}
