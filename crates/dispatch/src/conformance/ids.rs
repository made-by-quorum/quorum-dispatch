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

/// The lane a cell was measured on — the conformance matrix's lane axis. A
/// closed set (QS-3 uniformity: the same dimension list applies to every lane;
/// only per-cell applicability varies).
///
/// # This is a LANE, and a lane is `(harness, hosting)`
///
/// These five used to be provider ids, because a provider id used to BE a lane:
/// each of the five registered providers had exactly one topology, so
/// `"codex"` named a program and a hosting in one string. That is no longer
/// true — `quorum_qw::lane` carries nine lanes over four harnesses, and ACP is
/// a hosting (`Mode::Acp`) rather than a program — so a bare provider id names
/// a HARNESS and leaves the topology open. The battery's axis is not the
/// harness: `claude-code/mux-pane` and `claude-code/acp` are the same program
/// with different lifecycle, delivery, attach and transcript mechanisms, which
/// is precisely what D1–D7 measure. So each variant here names ONE lane, and
/// [`Lane::id`] — the serde rename, this type's wire surface — is that lane's
/// stable `quorum_qw` id. [`Lane::qw_lane`] is the same fact as a value, and
/// `conformance_lane_ids_are_real_qd_lanes` pins the two spellings together so
/// they cannot drift.
///
/// # Which five of the nine, and why those
///
/// The battery measures the lane its seeds actually drive, which is a fact
/// about the drivers in `harness.rs` and not a preference:
///
///   - `claude-code/mux-pane` — the bare claude TUI in a qd-owned pane; the
///     only lane in the matrix with a terminal, hence the only one D7 is
///     `Required` on.
///   - `claude-code/acp` — the same claude engine behind the `claude-code-acp`
///     bridge. Was `acp/claude-code`, a harness, and the rename is the whole
///     content of the change: same drivers, same cells, same bridge argv.
///   - `codex/daemon` — the headless codex resident. The C-2 D6 fixtures dial
///     a ws endpoint and fabricate a resident registry row; nothing in the
///     matrix opens a codex terminal.
///   - `pi/daemon` — likewise: the D6 fixtures fabricate a `pi-daemon` identity
///     row, and the D1 cells assert a qd-owned resident's pid.
///   - `opencode/acp` — opencode's only lane. Was `acp/opencode`.
///
/// The other four (`codex/mux-pane`, `codex/app-server`, `pi/mux-pane`,
/// `pi/extension`) are NOT in the matrix, and saying so is the point of putting
/// hosting on this axis: the battery never measured them, and while the axis
/// was provider-shaped it could not admit that — a `codex` tier read as a claim
/// about every codex topology. Adding one is a variant plus its drivers, and
/// nothing else here changes shape.
///
/// # Two of the harnesses are named without a suffix, deliberately
///
/// `ClaudeCode`/`ClaudeCodeAcp` carry their hosting in the variant name because
/// claude-code contributes TWO lanes and the bare name would be ambiguous.
/// `Codex`, `Pi` and `Opencode` contribute exactly one lane each, so the
/// harness name identifies it unambiguously; the id is where the hosting is
/// spelled. A second lane for any of them arrives with the suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lane {
    #[serde(rename = "claude-code/mux-pane")]
    ClaudeCode,
    #[serde(rename = "codex/daemon")]
    Codex,
    #[serde(rename = "claude-code/acp")]
    ClaudeCodeAcp,
    #[serde(rename = "opencode/acp")]
    Opencode,
    #[serde(rename = "pi/daemon")]
    Pi,
}

impl Lane {
    /// The five lanes the battery measures, in the published order. See the type
    /// docs for why these five of `quorum_qw::lane::Lane::ALL`'s nine.
    pub const ALL: [Lane; 5] = [
        Lane::ClaudeCode,
        Lane::Codex,
        Lane::ClaudeCodeAcp,
        Lane::Opencode,
        Lane::Pi,
    ];

    /// The stable lane id — the wire surface, byte-identical to the
    /// `#[serde(rename)]` above and to `quorum_qw::lane::Lane::id()`.
    ///
    /// This is the key every evidence artifact, applicability entry, grid cell
    /// key and observation id is written under, so it is what a stranger reads
    /// off the ledger. It is NOT a `--provider` argument any more — see
    /// [`Lane::harness_provider_id`] for that.
    pub fn id(self) -> &'static str {
        match self {
            Lane::ClaudeCode => "claude-code/mux-pane",
            Lane::Codex => "codex/daemon",
            Lane::ClaudeCodeAcp => "claude-code/acp",
            Lane::Opencode => "opencode/acp",
            Lane::Pi => "pi/daemon",
        }
    }

    /// The `quorum_qw` lane this measures — the authority on harness and
    /// hosting, and the operand every structural question about a lane goes
    /// through (`is_pane`, `is_daemon`, `harness`).
    ///
    /// Asking it here rather than re-deriving from the id string is what keeps
    /// the conformance grid from disagreeing with the dispatcher about what a
    /// lane IS: when the D7 applicability asks whether this lane has a terminal,
    /// it asks the same value `attach` and `kill` route on.
    pub fn qw_lane(self) -> quorum_qw::lane::Lane {
        use quorum_qw::lane::{Harness, Mode};
        let (harness, mode) = match self {
            Lane::ClaudeCode => (Harness::ClaudeCode, Mode::Pane),
            Lane::Codex => (Harness::Codex, Mode::Daemon),
            Lane::ClaudeCodeAcp => (Harness::ClaudeCode, Mode::Acp),
            Lane::Opencode => (Harness::Opencode, Mode::Acp),
            Lane::Pi => (Harness::Pi, Mode::Daemon),
        };
        quorum_qw::lane::Lane { harness, mode }
    }

    /// The harness's `--provider` argument — a bare program name, and NOT an
    /// identifier for this lane.
    ///
    /// Both claude lanes answer `"claude-code"` here, which is the honest shape:
    /// `--provider` selects the program, and the topology is selected by the
    /// flag beside it (`--acp`, `--daemon`, `--interactive`) or left to the
    /// harness default. Only the handful of drivers that assemble a real `qd`
    /// command line want this; everything that IDENTIFIES a lane wants
    /// [`Lane::id`].
    pub fn harness_provider_id(self) -> &'static str {
        self.qw_lane().harness.provider_id()
    }
}

#[cfg(test)]
mod lane_tests {
    use super::Lane;

    /// Every conformance lane names a lane qd actually has, spelled the way qd
    /// spells it. Two spellings of one fact (the `#[serde(rename)]`/[`Lane::id`]
    /// string and the [`Lane::qw_lane`] pair) can only stay honest if something
    /// compares them: without this, a renamed hosting token would leave the
    /// ledger publishing an id no dispatcher would recognise, and nothing would
    /// fail until a stranger tried to re-run the battery from the document.
    #[test]
    fn conformance_lane_ids_are_real_qd_lanes() {
        for lane in Lane::ALL {
            let qw = lane.qw_lane();
            assert_eq!(
                qw.id(),
                lane.id(),
                "{lane:?}'s published id and its qw lane disagree"
            );
            assert!(
                quorum_qw::lane::Lane::ALL.contains(&qw),
                "{lane:?} names {}, which is not one of qd's nine lanes",
                qw.id()
            );
            assert_eq!(
                quorum_qw::lane::Lane::from_id(lane.id()),
                Some(qw),
                "{} does not parse back to the lane it names",
                lane.id()
            );
        }
    }

    /// The matrix is a SUBSET of qd's lanes, and the four it omits are omitted
    /// because no seed drives them — not because they do not exist. Pinned so
    /// that adding a lane to `quorum_qw` does not silently read as "measured".
    #[test]
    fn the_matrix_is_five_distinct_lanes_of_the_nine() {
        let mut ids: Vec<&str> = Lane::ALL.iter().map(|l| l.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            Lane::ALL.len(),
            "two conformance lanes share an id"
        );
        assert!(
            Lane::ALL.len() < quorum_qw::lane::Lane::ALL.len(),
            "the battery claims to measure every lane qd has — if that became true, say so here rather than letting the assertion rot"
        );
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
    ///
    /// Keyed on [`Lane::id`], the LANE id, so two lanes of one harness never
    /// collide: `claude-code/mux-pane` and `claude-code/acp` measure the same
    /// cell ids on the same run, and under a harness-shaped key every one of
    /// those pairs would derive one observation id for two observations.
    pub fn derive(run: &RunId, lane: Lane, cell: &CellId) -> Self {
        ObservationId(format!("{}::{}::{}", run.0, lane.id(), cell.0))
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
