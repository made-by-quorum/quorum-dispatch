//! Identity newtypes for the conformance evidence schema (C-1).
//!
//! Every id is a thin newtype so the schema's function signatures are honest:
//! a `RunSequence` can never be passed where a `JournalOrdinal` is wanted, and
//! the tier computation (C-3) orders by `RunSequence` with `RunId` lexical
//! tie-break — types the compiler enforces, never a stringly-typed mixup.
//!
//! **Authority-issued values** (`RunSequence`, `CommissionToken`,
//! `AttributionSeq`, `JournalOrdinal`) are minted ONLY by the authority journal
//! / attribution register, never by cell authors or runners. Their constructors
//! are `pub(crate)` so nothing outside this module can forge one — the "issued
//! by the registrar, not the producer" property is structural, not a review
//! note. Production minting uses a random/ULID source injected as a
//! `&mut dyn FnMut() -> String` (the crate's existing `idstore::mint_*` pattern);
//! tests inject a deterministic sequence so the schema is reproducible.

use serde::{Deserialize, Serialize};

/// A battery-run identity (a ULID in production; any unique string in tests).
/// The tie-break in run ordering — vestigial under unique sequences, kept so
/// even invalid corpora order deterministically for validation reporting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(pub String);

/// The provider-lane a cell was measured on. A closed set (QS-3 uniformity: the
/// same dimension list applies to every lane; only per-cell applicability varies).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lane {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "acp/claude-code")]
    AcpClaudeCode,
    #[serde(rename = "opencode")]
    Opencode,
    #[serde(rename = "pi")]
    Pi,
}

impl Lane {
    /// The five registry lanes (`provider.rs:368-388`). `opencode` ≡ `acp/opencode`.
    pub const ALL: [Lane; 5] = [
        Lane::ClaudeCode,
        Lane::Codex,
        Lane::AcpClaudeCode,
        Lane::Opencode,
        Lane::Pi,
    ];

    /// The canonical provider id `qd start --provider <id>` resolves (matches
    /// `provider_for`). `Opencode` carries the internal `acp/opencode` id.
    pub fn provider_id(self) -> &'static str {
        match self {
            Lane::ClaudeCode => "claude-code",
            Lane::Codex => "codex",
            Lane::AcpClaudeCode => "acp/claude-code",
            Lane::Opencode => "acp/opencode",
            Lane::Pi => "pi",
        }
    }
}

/// A named box (host) a run executed on — windows are per (lane, box) (A9).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BoxId(pub String);

/// The authority-issued, strictly-increasing, unique-across-the-whole-journal
/// run sequence (F-3/N-1). Ordering is ALWAYS by this, never by timestamp.
/// Constructor is crate-private: only the journal issues one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunSequence(u64);

impl RunSequence {
    pub(crate) fn new(n: u64) -> Self {
        RunSequence(n)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A journal ordinal — the position of ANY entry (run-commissioning OR
/// designation) in the single ordered authority journal. Designation
/// pre-outcome standing (A9) is `run.sequence_ordinal > designation.ordinal`,
/// computable by integer comparison because both live in one domain (N-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JournalOrdinal(u64);

impl JournalOrdinal {
    pub(crate) fn new(n: u64) -> Self {
        JournalOrdinal(n)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An unforgeable registrar-issued token, unique per journal entry (F-3/N-1).
/// Recorded BEFORE any cell executes and bound into the run artifact's
/// commissioning header; a `Pass`/`Fail` proof-of-run carries it, so a green
/// cell is traceable to a pre-execution journal entry or fails validation (v3).
/// Constructor is crate-private: only the journal mints one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CommissionToken(String);

impl CommissionToken {
    pub(crate) fn new(s: impl Into<String>) -> Self {
        CommissionToken(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An immutable observation id (M-3): attribution rulings reference observations
/// by id, never by description. Derived deterministically from (run, lane, cell)
/// so it is unique within a run (A10 v5) and stable across serialization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationId(pub String);

impl ObservationId {
    /// Deterministic id for a (run, lane, cell) triple. Uniqueness within a run
    /// reduces to uniqueness of (lane, cell), which the registry guarantees.
    pub fn derive(run: &RunId, lane: Lane, cell: &CellId) -> Self {
        ObservationId(format!("{}::{}::{}", run.0, lane.provider_id(), cell.0))
    }
}

/// A cell id — a concrete test within a dimension (e.g. `d1.stop-reaps-pgid`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CellId(pub String);

/// An attribution-record identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordId(pub String);

/// The authority-issued, monotonic-per-issuance-register attribution sequence
/// (F-2). Precedence between records covering one observation is by this, with
/// `RecordId` tie-break — never by issuer timestamp. Crate-private constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AttributionSeq(u64);

impl AttributionSeq {
    pub(crate) fn new(n: u64) -> Self {
        AttributionSeq(n)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A content hash over the cell registry + per-lane applicability map the run
/// executed (M-1). A2's window eligibility keys on digest equality: any change
/// to the manifest starts a fresh window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ManifestDigest(pub String);

/// The aggregation-version identifier — the second thing A2's eligibility keys
/// on. Bumped when the aggregation semantics change; a bump starts a fresh window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AggregationVersion(pub String);

/// R9/R5-O3: a run is commissioned as `Evidence` (can enter a window) or
/// `Exercise` (a rehearsal/drill that can NEVER enter any window — A2(a)).
/// Also gates the commissioning-identity != runner-identity invariant (R6-6),
/// which binds only evidence-kind runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunKind {
    Evidence,
    Exercise,
}

/// The declared lane scope of a commissioned run (R5-O3): either one lane, or
/// an explicit COMPLETE set of lanes this run measures together. Per-(lane,
/// box) accounting operates on this declared scope, never on lanes inferred
/// from artifact contents. Canonical (sorted, deduped) so two readers of the
/// same scope agree byte-for-byte in the commissioning header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneScope(std::collections::BTreeSet<Lane>);

impl LaneScope {
    /// A single-lane scope.
    pub fn one(lane: Lane) -> Self {
        LaneScope([lane].into_iter().collect())
    }

    /// An explicit complete lane set. Rejects empty (a run must measure at
    /// least one lane) — there is no scope-less commissioning.
    pub fn complete(lanes: impl IntoIterator<Item = Lane>) -> Result<Self, String> {
        let set: std::collections::BTreeSet<Lane> = lanes.into_iter().collect();
        if set.is_empty() {
            return Err("lane scope: an explicit complete lane set cannot be empty".into());
        }
        Ok(LaneScope(set))
    }

    pub fn contains(&self, lane: Lane) -> bool {
        self.0.contains(&lane)
    }

    pub fn lanes(&self) -> impl Iterator<Item = Lane> + '_ {
        self.0.iter().copied()
    }
}

/// The registrar-minted, single-use launch nonce (R7-2): first disclosed by the
/// run-start event, unique across the journal, never derivable from the
/// commission token, the tuple, the run ID, or any earlier journal state.
/// Constructor is crate-private: only the journal's `start_run` mints one, at
/// the moment it appends the run-start event — so the runner cannot possess it
/// before that event exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LaunchNonce(String);

impl LaunchNonce {
    pub(crate) fn new(s: impl Into<String>) -> Self {
        LaunchNonce(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A content digest of a canonical run artifact (R6-1). The completion event
/// cites this digest; publication and terminalization are one atomic journal
/// append — there is no written-but-not-completed observation window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArtifactDigest(pub String);
