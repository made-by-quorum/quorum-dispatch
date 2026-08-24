//! The honesty spine (C-1). The one thing that makes this instrument worth
//! anything is that **a green cell cannot lie**. So the honesty is *structural*,
//! not reviewed-in:
//!
//! - There is no `pass: bool`. A green cell is [`Outcome::Pass`], which carries
//!   a [`ProofOfRun`] — and a `ProofOfRun` has private fields, no `Default`, and
//!   no public constructor. The ONLY producer is [`Runner`], created from a
//!   journal-issued [`CommissioningHeader`]. So a `Pass` is **unrepresentable**
//!   without a proof, and a proof is unrepresentable without a run that was
//!   commissioned in the authority journal before it executed. A cell that did
//!   not execute cannot serialize as passed. (See the `compile_fail` doctests.)
//! - A live gate that is OFF produces [`Outcome::blocked_gate_off`], never pass,
//!   by construction: [`Gate::run`]'s gate-off arm can only return `Blocked`.
//!   This retires the `acp_opencode_live.rs:90` / `codex_e2e_live.rs:326` no-op-
//!   pass trap class at the schema layer — a skipped run is `blocked(gate off)`,
//!   distinct from `pass`, distinct from a defect.
//! - A grid cell resolves to **exactly one** [`Observation`] XOR **exactly one**
//!   [`BoxCoverageRecord`] — never both, never neither (enforced by
//!   [`CellResolution`] being a two-variant enum and the builder requiring one
//!   per applicable cell). A `BoxCoverageRecord` is a *distinct type* with no
//!   conversion to (or from) pass/fail/blocked/na, so a cross-box gap can never
//!   masquerade as an observed outcome.
//!
//! The four unconfusable states the plan demands:
//! `Blocked` (a genuine per-box gate: gate named + re-entry), `NotApplicable`
//! (a declared reason), a cell with **no working implementation** (a C-1 *defect*
//! — it simply has no resolution, which the builder rejects as an incomplete
//! grid; never silently `blocked`), and a `BoxCoverageRecord` (runnable only on
//! another box). Four distinct representations, no coercion between them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ids::{
    AggregationVersion, BoxId, CellId, CommissionToken, Lane, LaneScope, LaunchNonce,
    ManifestDigest, ObservationId, RunId, RunKind, RunSequence,
};

/// How a cell's observation was produced — automated vs a scripted-manual
/// runbook (screen/zellij). A `ScriptedManual` pass counts in the aggregate but
/// GA's evidence-run gating clause (A7, C-3) requires `Automated`; carrying the
/// label here keeps the automation debt visible instead of laundering into green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunMode {
    #[serde(rename = "automated")]
    Automated,
    #[serde(rename = "scripted-manual")]
    ScriptedManual,
}

/// Proof that a cell actually executed. **Cannot be constructed with empty
/// evidence and cannot be constructed outside a commissioned [`Runner`].**
///
/// Fields are private, there is no `Default`, and the only constructor is
/// [`Runner::proof`], which asserts non-empty evidence and stamps the runner's
/// journal-issued [`CommissionToken`]. That token is what binds a green cell to
/// a pre-execution journal entry (A2(e)/v3, checked at corpus scope by C-3's
/// A10): a `Pass` you hand-wrote in JSON with an empty or unregistered token
/// fails that check — so the accidental/lazy fabrication is unrepresentable
/// here, and the validation-surviving fabrication would require forging a
/// journal entry, which the registrar exclusions + append-time integrity
/// (see [`super::journal`]) prevent.
///
/// ```compile_fail
/// # use dispatch::conformance::evidence::ProofOfRun;
/// // Private fields — cannot be constructed outside the module:
/// let _ = ProofOfRun { commission_token: todo!(), commands: vec![], observed: String::new() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofOfRun {
    /// The commission token of the run this proof was minted under.
    commission_token: CommissionToken,
    /// The launch nonce this proof was minted under (R7-2). Every green cell's
    /// proof-of-run carries it — a proof with no nonce binding is unrepresentable
    /// (the field is required, not `Option`), and a proof whose nonce does not
    /// match the artifact header's is a defect [`RunArtifactBuilder::build`]
    /// rejects.
    launch_nonce: LaunchNonce,
    /// The exact commands executed (non-empty by construction).
    commands: Vec<String>,
    /// The observed source state the verdict rests on (non-empty by construction).
    observed: String,
}

impl ProofOfRun {
    /// The token binding this proof to its pre-execution journal entry (v3).
    pub fn commission_token(&self) -> &CommissionToken {
        &self.commission_token
    }
    /// The launch nonce binding this proof to assembly-after-start (R7-2/v8).
    pub fn launch_nonce(&self) -> &LaunchNonce {
        &self.launch_nonce
    }
    pub fn commands(&self) -> &[String] {
        &self.commands
    }
    pub fn observed(&self) -> &str {
        &self.observed
    }
}

/// A cell's outcome. **No `pass: bool`.** The two *executed* outcomes (`Pass`,
/// `Fail`) each carry a [`ProofOfRun`]; the two *not-executed* outcomes
/// (`Blocked`, `NotApplicable`) carry declared reasons. There is deliberately no
/// `Default` — an outcome must be authored as exactly one of these.
///
/// ```compile_fail
/// # use dispatch::conformance::evidence::{Outcome, ProofOfRun};
/// // A Pass REQUIRES a ProofOfRun; there is no `Outcome::Pass(true)` and no
/// // proof you can build outside a commissioned Runner:
/// let _ = Outcome::Pass(ProofOfRun::default());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// The cell ran and its dimension's claim held. Carries proof-of-run.
    Pass(ProofOfRun),
    /// The cell ran and its claim did not hold. Carries proof-of-run + detail.
    Fail { proof: ProofOfRun, detail: String },
    /// The cell did not run because a genuine per-box gate is closed. Names the
    /// gate and the re-entry condition. NEVER produced for a missing impl.
    Blocked { reason: String, reentry: String },
    /// The cell does not apply here, with a declared, stranger-acceptable reason.
    NotApplicable { reason: String },
}

impl Outcome {
    /// A blocked outcome for a closed gate (the general per-box gate case).
    pub fn blocked(reason: impl Into<String>, reentry: impl Into<String>) -> Outcome {
        Outcome::Blocked {
            reason: reason.into(),
            reentry: reentry.into(),
        }
    }

    /// The canonical "live gate is OFF" blocked outcome — the no-op-pass retiree.
    /// A skipped live run lands HERE, never at `Pass`.
    pub fn blocked_gate_off(
        gate_var: impl std::fmt::Display,
        reentry: impl Into<String>,
    ) -> Outcome {
        Outcome::Blocked {
            reason: format!("gate off: {gate_var} unset"),
            reentry: reentry.into(),
        }
    }

    pub fn not_applicable(reason: impl Into<String>) -> Outcome {
        Outcome::NotApplicable {
            reason: reason.into(),
        }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass(_))
    }
    pub fn is_fail(&self) -> bool {
        matches!(self, Outcome::Fail { .. })
    }
    pub fn is_blocked(&self) -> bool {
        matches!(self, Outcome::Blocked { .. })
    }

    /// The proof-of-run for an executed outcome (`Pass`/`Fail`); `None` for the
    /// not-executed outcomes. C-3's v3 reads this to bind green to the journal.
    pub fn proof(&self) -> Option<&ProofOfRun> {
        match self {
            Outcome::Pass(p) => Some(p),
            Outcome::Fail { proof, .. } => Some(proof),
            _ => None,
        }
    }
}

/// One per-cell observation: an immutable id, its (lane, cell), how it was run,
/// and its outcome. Attribution rulings reference it by [`ObservationId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub id: ObservationId,
    pub lane: Lane,
    pub cell: CellId,
    pub run_mode: RunMode,
    pub outcome: Outcome,
}

/// A typed cross-box coverage record (M-5) — **outside the run-outcome union**.
/// Declares that an applicable cell was not run here because it is runnable only
/// on another named box, with provenance and a re-entry condition. It is a
/// distinct type: there is no `From`/`Into` between it and [`Outcome`], so it
/// can never become — or be read as — pass, fail, blocked, or n-a. Its tier
/// effects (all downward) live in C-3's A9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxCoverageRecord {
    pub run: RunId,
    pub lane: Lane,
    pub cell: CellId,
    /// The box this cell IS runnable on.
    pub runnable_on: BoxId,
    /// Why it is not runnable on the box this run executed on.
    pub why_not_here: String,
    /// Who established this coverage fact (instance name + commission id).
    pub established_by: String,
    /// The condition under which this box could run the cell (re-entry).
    pub reentry: String,
}

impl BoxCoverageRecord {
    /// Structural well-formedness, symmetric with
    /// [`super::attribution::AttributionRecord::well_formed`]: the non-`Option`
    /// fields make a missing field unrepresentable, and this catches the
    /// emptiness the type cannot — an empty runnable-on box, why-not-here,
    /// establishing authority, or re-entry condition is malformed, never
    /// silently serialized as a clean coverage record. Enforced at the builder
    /// boundary ([`RunArtifactBuilder::build`]), so a malformed coverage record
    /// is loudly invalid, not quietly accepted.
    pub fn well_formed(&self) -> Result<(), String> {
        if self.runnable_on.0.trim().is_empty() {
            return Err(format!(
                "coverage record for cell {:?}: empty runnable-on box",
                self.cell.0
            ));
        }
        if self.why_not_here.trim().is_empty() {
            return Err(format!(
                "coverage record for cell {:?}: empty why-not-here",
                self.cell.0
            ));
        }
        if self.established_by.trim().is_empty() {
            return Err(format!(
                "coverage record for cell {:?}: empty establishing authority",
                self.cell.0
            ));
        }
        if self.reentry.trim().is_empty() {
            return Err(format!(
                "coverage record for cell {:?}: empty re-entry condition",
                self.cell.0
            ));
        }
        Ok(())
    }
}

/// A grid cell resolves to **exactly one** of these — never both, never neither.
/// The builder ([`RunArtifactBuilder`]) requires one resolution per
/// manifest-applicable (lane, cell); an unresolved applicable cell is a C-1
/// *defect* (an incomplete grid), NOT a silent `blocked`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CellResolution {
    Observed(Observation),
    Coverage(BoxCoverageRecord),
}

/// The immutable commissioning-context tuple (R5-O3), minted BEFORE any cell
/// executes by [`super::journal::AuthorityJournal::commission_run`]: everything
/// a window-eligibility check needs, bound at commissioning so no per-(lane,
/// box) accounting is ever inferred from artifact contents. Planned domain is
/// deliberately NOT a stored field here — R9 requires it "derived from the pair
/// [lane scope, manifest digest]", so a second free-floating domain source can
/// never conflict with the registry; derive it via
/// `Battery::applicable_domain` filtered to `lane_scope`, at `manifest_digest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissioningTuple {
    pub run: RunId,
    pub lane_scope: LaneScope,
    pub box_id: BoxId,
    pub release_commit: String,
    pub manifest_digest: ManifestDigest,
    pub aggregation_version: AggregationVersion,
    /// `Exercise`-kind runs can never enter any window (A2(a)/R5-O3).
    pub run_kind: RunKind,
    pub sequence: RunSequence,
    pub commissioning_identity: String,
    pub token: CommissionToken,
}

/// The structural commissioning header (F-3/N-1/R7-2): the immutable tuple
/// EXACTLY, byte-comparable, plus exactly one start-derived field — the launch
/// nonce, reproducing the run-start event's nonce exactly. It is a **required,
/// non-optional** field of every [`RunArtifact`], and observations live INSIDE
/// the artifact — so an artifact cannot serialize any observation without its
/// header. Token-before-start and assembly-after-start are therefore checkable
/// from the artifact + journal pair, never asserted (R7-2/R8-1) —
/// execution-after-start remains ASSERTED under the cooperative-agents trust
/// model (Trust model, R8-1), which this header does not and cannot check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissioningHeader {
    pub tuple: CommissioningTuple,
    pub launch_nonce: LaunchNonce,
}

impl CommissioningHeader {
    /// Assemble the header from the two authority-issued halves: the
    /// commissioning tuple ([`super::journal::AuthorityJournal::commission_run`])
    /// and the launch nonce
    /// ([`super::journal::AuthorityJournal::start_run`]). Both inputs are
    /// already unforgeable by construction (crate-private minting); this is
    /// pure assembly, not a new authority act.
    pub fn new(tuple: CommissioningTuple, launch_nonce: LaunchNonce) -> Self {
        CommissioningHeader {
            tuple,
            launch_nonce,
        }
    }

    pub fn run(&self) -> &RunId {
        &self.tuple.run
    }
    pub fn sequence(&self) -> RunSequence {
        self.tuple.sequence
    }
    pub fn token(&self) -> &CommissionToken {
        &self.tuple.token
    }
}

/// The only producer of a [`ProofOfRun`] — hence the only producer of a `Pass`
/// or `Fail`. Constructed from a [`CommissioningHeader`] (via
/// [`RunArtifactBuilder`]), so you cannot mint proof without a journal entry
/// that issued the token before execution AND a run-start event that issued the
/// nonce before any cell executes (R7-2) — the harness injects this nonce into
/// every cell execution.
#[derive(Debug, Clone)]
pub struct Runner {
    token: CommissionToken,
    launch_nonce: LaunchNonce,
}

impl Runner {
    /// Mint proof of a real run. Empty commands or empty observed state is not a
    /// proof — it is rejected, so a "green" with nothing behind it is
    /// unrepresentable even from inside the crate.
    pub fn proof(&self, commands: Vec<String>, observed: impl Into<String>) -> ProofOfRun {
        let observed = observed.into();
        assert!(
            !commands.is_empty(),
            "ProofOfRun requires the commands that were run (a green cell must show its work)"
        );
        assert!(
            !observed.trim().is_empty(),
            "ProofOfRun requires the observed source state the verdict rests on"
        );
        ProofOfRun {
            commission_token: self.token.clone(),
            launch_nonce: self.launch_nonce.clone(),
            commands,
            observed,
        }
    }

    /// A passing outcome, carrying proof.
    pub fn pass(&self, commands: Vec<String>, observed: impl Into<String>) -> Outcome {
        Outcome::Pass(self.proof(commands, observed))
    }

    /// A failing outcome, carrying proof + a detail line.
    pub fn fail(
        &self,
        commands: Vec<String>,
        observed: impl Into<String>,
        detail: impl Into<String>,
    ) -> Outcome {
        Outcome::Fail {
            proof: self.proof(commands, observed),
            detail: detail.into(),
        }
    }
}

/// A live-execution gate (e.g. `QD_PI_LIVE`). The gate-off arm of [`Gate::run`]
/// can only return `Blocked` — there is no code path from "gate off" to `Pass`.
pub enum Gate {
    On,
    Off { var: String },
}

impl Gate {
    /// Read a gate from the environment: ON iff `var` is set to a truthy value
    /// (`1`/`true`/`yes`, case-insensitive), OFF otherwise.
    pub fn from_env(var: &str) -> Gate {
        match std::env::var(var) {
            Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes") => {
                Gate::On
            }
            _ => Gate::Off {
                var: var.to_string(),
            },
        }
    }

    /// Run `body` (with a [`Runner`]) only when the gate is ON; otherwise return
    /// `Blocked{gate off}` by construction. This is the structural retirement of
    /// the no-op-pass trap: a skipped live run is `blocked`, never `pass`.
    pub fn run(
        self,
        runner: &Runner,
        reentry: impl Into<String>,
        body: impl FnOnce(&Runner) -> Outcome,
    ) -> Outcome {
        match self {
            Gate::On => body(runner),
            Gate::Off { var } => Outcome::blocked_gate_off(var, reentry),
        }
    }
}

/// A full battery-run evidence artifact (T3). Machine- and human-readable; the
/// tier computation (C-3) reads window-eligibility operands (manifest digest,
/// aggregation version, journal sequence + commission token) and the
/// coverage/attribution records straight off it, with no out-of-band knowledge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunArtifact {
    /// Required commissioning header — no observation exists without it. Every
    /// tuple field the tier computation reads (commit, box, lane scope, manifest
    /// digest, aggregation version, run kind, sequence, token) lives here — the
    /// artifact carries no second, independently-settable copy of any of them.
    pub header: CommissioningHeader,
    /// Descriptive timestamp ONLY — ordering is by sequence, never timestamp.
    pub timestamp: String,
    /// The grid: each applicable (lane, cell) → exactly one resolution.
    pub grid: BTreeMap<String, CellResolution>,
}

impl RunArtifact {
    /// The grid-map key for a (lane, cell): the LANE id, not its harness, so
    /// the two lanes of one harness hold distinct rows in the same grid.
    /// Stable + human-readable.
    pub fn cell_key(lane: Lane, cell: &CellId) -> String {
        format!("{}::{}", lane.id(), cell.0)
    }

    /// Every observation in the grid (skips coverage records).
    pub fn observations(&self) -> impl Iterator<Item = &Observation> {
        self.grid.values().filter_map(|r| match r {
            CellResolution::Observed(o) => Some(o),
            CellResolution::Coverage(_) => None,
        })
    }

    /// The canonical content digest of this artifact (R6-1): a stable SHA-256
    /// over the artifact's canonical JSON. Two readers of the same artifact
    /// derive the same digest, so a completion event's cited digest binds
    /// artifact content — the operand C-3's v2 (the cited digest must be present
    /// in the index) and the index head digest read against it. The `grid` is a
    /// `BTreeMap` (key-sorted) and every field is deterministic, so this is
    /// order-independent.
    pub fn content_digest(&self) -> super::ids::ArtifactDigest {
        use sha2::{Digest, Sha256};
        let json = serde_json::to_vec(self).expect("run artifact serializes");
        super::ids::ArtifactDigest(format!("sha256:{:x}", Sha256::digest(&json)))
    }
}

/// Assembles a [`RunArtifact`] from a commissioned header. Owns the [`Runner`],
/// so cells produce `Pass`/`Fail` only through it. `build` enforces fullness
/// over the manifest-applicable domain (A2(d)): every applicable (lane, cell)
/// has exactly one resolution — an unresolved applicable cell is a C-1 defect,
/// surfaced as an error, never a silent blocked. `build` also enforces that
/// every executed outcome's proof-of-run carries THIS header's token and nonce
/// — a proof stitched in from another run's `Runner` is a defect, loud, never a
/// silent cross-run splice.
pub struct RunArtifactBuilder {
    header: CommissioningHeader,
    runner: Runner,
    timestamp: String,
    grid: BTreeMap<String, CellResolution>,
}

impl RunArtifactBuilder {
    /// Create a builder from a journal-issued header (see
    /// [`super::journal::AuthorityJournal::commission_run`] +
    /// [`super::journal::AuthorityJournal::start_run`], combined via
    /// [`CommissioningHeader::new`]).
    pub fn new(header: CommissioningHeader, timestamp: impl Into<String>) -> Self {
        let runner = Runner {
            token: header.tuple.token.clone(),
            launch_nonce: header.launch_nonce.clone(),
        };
        RunArtifactBuilder {
            header,
            runner,
            timestamp: timestamp.into(),
            grid: BTreeMap::new(),
        }
    }

    /// The run's id — cells derive their observation id from it.
    pub fn run_id(&self) -> &RunId {
        &self.header.tuple.run
    }

    /// The runner cells use to mint `Pass`/`Fail` (or to feed a [`Gate`]).
    pub fn runner(&self) -> &Runner {
        &self.runner
    }

    /// Record an observed outcome for a (lane, cell). The observation id is
    /// derived from (run, lane, cell) so it is unique within the run.
    pub fn observe(&mut self, lane: Lane, cell: CellId, run_mode: RunMode, outcome: Outcome) {
        let id = ObservationId::derive(&self.header.tuple.run, lane, &cell);
        let key = RunArtifact::cell_key(lane, &cell);
        self.grid.insert(
            key,
            CellResolution::Observed(Observation {
                id,
                lane,
                cell,
                run_mode,
                outcome,
            }),
        );
    }

    /// Record a cross-box coverage record for a (lane, cell).
    pub fn cover(&mut self, record: BoxCoverageRecord) {
        let key = RunArtifact::cell_key(record.lane, &record.cell);
        self.grid.insert(key, CellResolution::Coverage(record));
    }

    /// Finalize. `applicable` is the manifest-authorized (lane, cell) domain
    /// this run must fully resolve; any applicable cell without a resolution is
    /// an incomplete grid — a C-1 defect — reported loudly, never silent.
    pub fn build(self, applicable: &[(Lane, CellId)]) -> Result<RunArtifact, String> {
        let mut missing = Vec::new();
        for (lane, cell) in applicable {
            let key = RunArtifact::cell_key(*lane, cell);
            if !self.grid.contains_key(&key) {
                missing.push(key);
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "incomplete grid (C-1 defect: {} applicable cell(s) with no resolution — never silently defaulted): {}",
                missing.len(),
                missing.join(", ")
            ));
        }
        // Every coverage record must be well-formed (symmetric with attribution
        // well-formedness) — a coverage record with an empty why-not-here /
        // re-entry / authority / runnable-on box is malformed and loudly
        // rejected here, never quietly serialized as a clean cross-box gap.
        let mut malformed = Vec::new();
        for resolution in self.grid.values() {
            if let CellResolution::Coverage(cov) = resolution {
                if let Err(e) = cov.well_formed() {
                    malformed.push(e);
                }
            }
        }
        if !malformed.is_empty() {
            return Err(format!(
                "malformed coverage record(s) (unrepresentable/loudly invalid, symmetric with attribution well-formedness): {}",
                malformed.join("; ")
            ));
        }
        // Every executed outcome's proof must be bound to THIS header's token +
        // nonce — a green cell whose proof was minted under a different run's
        // Runner is a defect, never a silent cross-run splice (R7-2/v3/v8).
        let mut mismatched = Vec::new();
        for resolution in self.grid.values() {
            if let CellResolution::Observed(obs) = resolution {
                if let Some(proof) = obs.outcome.proof() {
                    if proof.commission_token() != &self.header.tuple.token {
                        mismatched.push(format!("{}: commission token mismatch", obs.id.0));
                    }
                    if proof.launch_nonce() != &self.header.launch_nonce {
                        mismatched.push(format!("{}: launch nonce mismatch", obs.id.0));
                    }
                }
            }
        }
        if !mismatched.is_empty() {
            return Err(format!(
                "grid carries proof-of-run not bound to this artifact's header (unrepresentable/loudly invalid, R7-2): {}",
                mismatched.join(", ")
            ));
        }
        Ok(RunArtifact {
            header: self.header,
            timestamp: self.timestamp,
            grid: self.grid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::ids::{CellId, LaneScope, RunId, RunKind};
    use crate::conformance::journal::AuthorityJournal;

    // A test helper: commission AND start a run through the journal (the ONLY
    // way to get a header, hence the only way to get a Runner, hence the only
    // way to a Pass) — the two-phase mint the real registrar performs.
    fn commission(run: &str) -> (AuthorityJournal, CommissioningHeader) {
        let mut journal = AuthorityJournal::new();
        let mut n = 0u64;
        let mut gen_tok = || {
            n += 1;
            format!("tok-{n}")
        };
        let tuple = journal
            .commission_run(
                RunId(run.into()),
                LaneScope::one(Lane::Pi),
                BoxId("lima".into()),
                "commit-abc",
                ManifestDigest("sha256:deadbeef".into()),
                AggregationVersion("agg-v1".into()),
                RunKind::Evidence,
                "registrar-1 (uq5xx83a)",
                &mut gen_tok,
            )
            .unwrap();
        let mut m = 0u64;
        let mut gen_nonce = || {
            m += 1;
            format!("nonce-{run}-{m}")
        };
        let nonce = journal
            .start_run(&tuple.run.clone(), "runner-1 (uq5xx83a)", &mut gen_nonce)
            .unwrap();
        let header = CommissioningHeader::new(tuple, nonce);
        (journal, header)
    }

    fn builder(header: CommissioningHeader) -> RunArtifactBuilder {
        RunArtifactBuilder::new(header, "2026-07-13T00:00:00Z")
    }

    #[test]
    fn no_op_pass_is_blocked_not_green() {
        // The trap this whole schema retires: a live gate that is OFF.
        let (_j, header) = commission("r1");
        let b = builder(header);
        let outcome = Gate::Off {
            var: "QD_PI_LIVE".into(),
        }
        .run(b.runner(), "set QD_PI_LIVE=1 on a box with pi creds", |r| {
            // If this body ran, we'd get a pass — it must NOT run when gate off.
            r.pass(vec!["unreachable".into()], "unreachable")
        });
        assert!(
            outcome.is_blocked(),
            "gate-off must be blocked, got {outcome:?}"
        );
        assert!(!outcome.is_pass(), "gate-off must NEVER be pass");
        match outcome {
            Outcome::Blocked { reason, .. } => assert!(reason.contains("gate off")),
            _ => panic!("expected blocked"),
        }
    }

    #[test]
    fn gate_on_runs_body_and_passes_with_proof() {
        let (_j, header) = commission("r2");
        let tok = header.token().clone();
        let b = builder(header);
        let outcome = Gate::On.run(b.runner(), "n/a", |r| {
            r.pass(
                vec!["qd start s --provider pi".into()],
                "row written; pid alive",
            )
        });
        assert!(outcome.is_pass());
        // The pass carries proof bound to the run's commission token.
        assert_eq!(outcome.proof().unwrap().commission_token(), &tok);
    }

    #[test]
    #[should_panic(expected = "commands")]
    fn proof_rejects_empty_commands() {
        let (_j, header) = commission("r3");
        let b = builder(header);
        let _ = b.runner().proof(vec![], "observed something");
    }

    #[test]
    #[should_panic(expected = "observed")]
    fn proof_rejects_empty_observed() {
        let (_j, header) = commission("r4");
        let b = builder(header);
        let _ = b.runner().proof(vec!["did a thing".into()], "   ");
    }

    #[test]
    fn incomplete_grid_is_a_defect_never_blocked() {
        // An applicable cell with no resolution is a C-1 defect surfaced loudly —
        // it is NOT silently converted to blocked.
        let (_j, header) = commission("r5");
        let mut b = builder(header);
        let c1 = CellId("d1.stop-reaps-pgid".into());
        let c2 = CellId("d2.turn-accepted".into());
        // resolve only c1; leave c2 unresolved
        b.observe(
            Lane::Pi,
            c1.clone(),
            RunMode::Automated,
            Gate::On.run(b.runner(), "n/a", |r| {
                r.pass(vec!["qd stop s".into()], "pid gone; row tombstoned")
            }),
        );
        let applicable = vec![(Lane::Pi, c1), (Lane::Pi, c2)];
        let err = b.build(&applicable).unwrap_err();
        assert!(err.contains("defect"), "must name it a defect: {err}");
        assert!(!err.contains("blocked"), "must NOT resolve it as blocked");
        assert!(err.contains("d2.turn-accepted"));
    }

    #[test]
    fn coverage_is_outside_the_outcome_union() {
        // A BoxCoverageRecord is a distinct type — it resolves a cell but is never
        // an Observation/Outcome, and observations() skips it.
        let (_j, header) = commission("r6");
        let mut b = builder(header);
        let cell = CellId("d7.nested-mux-tmux".into());
        b.cover(BoxCoverageRecord {
            run: b.run_id().clone(),
            lane: Lane::Pi,
            cell: cell.clone(),
            runnable_on: BoxId("brano".into()),
            why_not_here: "no display surface on lima headless".into(),
            established_by: "coord-1 (uq5xx83a)".into(),
            reentry: "run on a box with a real terminal display".into(),
        });
        let artifact = b.build(&[(Lane::Pi, cell.clone())]).unwrap();
        // The cell resolved, but produced NO observation.
        assert_eq!(artifact.observations().count(), 0);
        let key = RunArtifact::cell_key(Lane::Pi, &cell);
        assert!(matches!(
            artifact.grid.get(&key),
            Some(CellResolution::Coverage(_))
        ));
    }

    #[test]
    fn build_rejects_a_cross_run_proof_splice() {
        // S2: the negative path the checklist names. A Pass minted under run A's
        // Runner, spliced into run B's grid, must be rejected by build() — token
        // AND nonce are bound to the header, so a cross-run proof is a loud
        // defect, never a silent splice (R7-2/v3/v8). Two distinct commissions in
        // ONE journal → distinct tokens AND distinct nonces, so both mismatch
        // branches fire.
        let mut journal = AuthorityJournal::new();
        let mut tn = 0u64;
        let mut gen_tok = || {
            tn += 1;
            format!("tok-{tn}")
        };
        let tuple_a = journal
            .commission_run(
                RunId("run-a".into()),
                LaneScope::one(Lane::Pi),
                BoxId("lima".into()),
                "commit-abc",
                ManifestDigest("sha256:deadbeef".into()),
                AggregationVersion("agg-v1".into()),
                RunKind::Evidence,
                "registrar-1 (uq5xx83a)",
                &mut gen_tok,
            )
            .unwrap();
        let tuple_b = journal
            .commission_run(
                RunId("run-b".into()),
                LaneScope::one(Lane::Pi),
                BoxId("lima".into()),
                "commit-abc",
                ManifestDigest("sha256:deadbeef".into()),
                AggregationVersion("agg-v1".into()),
                RunKind::Evidence,
                "registrar-1 (uq5xx83a)",
                &mut gen_tok,
            )
            .unwrap();
        let mut nn = 0u64;
        let mut gen_nonce = || {
            nn += 1;
            format!("nonce-{nn}")
        };
        let nonce_a = journal
            .start_run(&tuple_a.run.clone(), "runner-a (uq5xx83a)", &mut gen_nonce)
            .unwrap();
        let nonce_b = journal
            .start_run(&tuple_b.run.clone(), "runner-b (uq5xx83a)", &mut gen_nonce)
            .unwrap();
        let header_a = CommissioningHeader::new(tuple_a, nonce_a);
        let header_b = CommissioningHeader::new(tuple_b, nonce_b);

        let cell = CellId("d1.boot-readiness".into());
        // Mint a real proof under run A's runner.
        let builder_a = RunArtifactBuilder::new(header_a, "2026-07-13T00:00:00Z");
        let foreign_pass = builder_a.runner().pass(
            vec!["qd start s --provider pi".into()],
            "row present; pid alive",
        );

        // Splice A's proof into run B's grid.
        let mut builder_b = RunArtifactBuilder::new(header_b, "2026-07-13T00:00:00Z");
        builder_b.observe(Lane::Pi, cell.clone(), RunMode::Automated, foreign_pass);
        let err = builder_b.build(&[(Lane::Pi, cell)]).unwrap_err();
        assert!(err.contains("commission token mismatch"), "{err}");
        assert!(err.contains("launch nonce mismatch"), "{err}");
    }

    #[test]
    fn build_rejects_a_malformed_coverage_record() {
        // S3: BoxCoverageRecord::well_formed is enforced at the builder boundary —
        // an empty why-not-here is malformed and loudly rejected, symmetric with
        // attribution well-formedness (not silently serialized clean).
        let (_j, header) = commission("r-cov");
        let mut b = builder(header);
        let cell = CellId("d7.nested-mux-tmux".into());
        b.cover(BoxCoverageRecord {
            run: b.run_id().clone(),
            lane: Lane::Pi,
            cell: cell.clone(),
            runnable_on: BoxId("brano".into()),
            why_not_here: "   ".into(), // malformed-by-emptiness
            established_by: "coord-1 (uq5xx83a)".into(),
            reentry: "run on a box with a real terminal display".into(),
        });
        let err = b.build(&[(Lane::Pi, cell)]).unwrap_err();
        assert!(err.contains("malformed coverage record"), "{err}");
        assert!(err.contains("why-not-here"), "{err}");
    }

    #[test]
    fn box_coverage_record_well_formed_catches_each_empty_field() {
        let base = BoxCoverageRecord {
            run: RunId("r".into()),
            lane: Lane::Pi,
            cell: CellId("d7.nested-mux-tmux".into()),
            runnable_on: BoxId("brano".into()),
            why_not_here: "no display surface".into(),
            established_by: "coord-1 (uq5xx83a)".into(),
            reentry: "run on a box with a display".into(),
        };
        base.well_formed().unwrap();
        let mut r = base.clone();
        r.runnable_on = BoxId("  ".into());
        assert!(r.well_formed().unwrap_err().contains("runnable-on"));
        let mut r = base.clone();
        r.established_by = "".into();
        assert!(r
            .well_formed()
            .unwrap_err()
            .contains("establishing authority"));
        let mut r = base.clone();
        r.reentry = " ".into();
        assert!(r.well_formed().unwrap_err().contains("re-entry"));
    }

    #[test]
    fn artifact_serializes_and_round_trips_with_header() {
        let (_j, header) = commission("r7");
        let mut b = builder(header);
        let cell = CellId("d1.start-visible".into());
        b.observe(
            Lane::Pi,
            cell.clone(),
            RunMode::Automated,
            Gate::On.run(b.runner(), "n/a", |r| {
                r.pass(vec!["qd start s".into()], "row present; info resolves")
            }),
        );
        let artifact = b.build(&[(Lane::Pi, cell)]).unwrap();
        let json = serde_json::to_string_pretty(&artifact).unwrap();
        // The commissioning header (run/sequence/token) is structural in the JSON.
        assert!(json.contains("\"sequence\""));
        assert!(json.contains("\"token\""));
        let back: RunArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(back, artifact);
    }
}
