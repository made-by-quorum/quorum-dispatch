//! (a) The fail-closed conformance grid-minting gate — the missing choke point.
//!
//! C-4 (a) hardening. Before this, there was no aggregator that turned a run's
//! per-cell evidence into a certifiable grid: the engine battery's
//! `write_result` wrote per-row files that nothing read back, and a box where
//! the live planes never ran produced a green `cargo test` with zero proof —
//! "green from a box where nothing ran". This module mints THE grid: a
//! completeness + proof-of-run gate over the assembled [`RunArtifact`]s,
//! **fail-closed** — a grid is `certified` only when every manifest in-domain
//! `(lane, cell)` reaches a *satisfying* resolution (a real [`Outcome::Pass`]
//! where the manifest marks the cell [`Applicability::Required`]), and INVALID
//! otherwise, **regardless of any flag**. Absence of proof mints INVALID by
//! construction, so a green grid cannot be minted from a box where nothing ran;
//! safety does not depend on opting into a strict mode.
//!
//! It leans on C-1's honesty spine and does NOT reinvent it: a [`Outcome::Pass`]
//! is unrepresentable without a [`super::evidence::ProofOfRun`] minted by a
//! commissioned [`super::evidence::Runner`], so this gate need only require the
//! *presence of a Pass* per required cell — the Pass's authenticity is already
//! structural. Distinct from C-3's `tier` (which computes a rate/tier over the
//! post-C-5 measured corpus): this computes no rate; it is the raw-battery
//! proof-of-run completeness gate (the release grid), the instrument C-5 wields
//! to mint the first MEASURED grid. Its output is a deterministic machine
//! artifact ([`GridVerdict::render`]) a downstream reader consumes unchanged.

use std::path::Path;

use super::evidence::{CellResolution, Outcome, RunArtifact};
use super::ids::{CellId, Lane};
use super::registry::{Applicability, Battery};

/// Why a `(lane, cell)` is certifiable — or why it is not. The three
/// certifiable variants are the only ways a green grid is mintable; every other
/// variant forces the grid to INVALID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellStatus {
    /// A real measured green — an [`Outcome::Pass`] carrying proof-of-run.
    Passed,
    /// The manifest declares this cell not-applicable and it resolved so.
    NotApplicableDeclared,
    /// The manifest permits cross-box coverage and a valid coverage record was present.
    CoveredElsewhere,
    /// NOT certifiable — the cell measured red.
    Failed(String),
    /// NOT certifiable — the cell did not measure (a skipped live plane or a
    /// genuine per-box gate both serialize [`Outcome::Blocked`]). This is the
    /// fail-closed heart: a box where the plane never ran lands every required
    /// cell HERE, and a Blocked is not a Pass.
    Blocked(String),
    /// NOT certifiable — no resolution at all in the assembled evidence.
    Missing,
    /// NOT certifiable — resolved in a way the manifest does not permit.
    NotPermitted,
}

impl CellStatus {
    /// A grid is `certified` only if EVERY in-domain cell is certifiable.
    pub fn is_certifiable(&self) -> bool {
        matches!(
            self,
            CellStatus::Passed | CellStatus::NotApplicableDeclared | CellStatus::CoveredElsewhere
        )
    }

    fn label(&self) -> String {
        match self {
            CellStatus::Passed => "PASS".to_string(),
            CellStatus::NotApplicableDeclared => "N/A (declared)".to_string(),
            CellStatus::CoveredElsewhere => "COVERED-ELSEWHERE".to_string(),
            CellStatus::Failed(d) => format!("FAIL: {d}"),
            CellStatus::Blocked(r) => format!("BLOCKED: {r}"),
            CellStatus::Missing => "MISSING (no resolution — nothing ran)".to_string(),
            CellStatus::NotPermitted => "NOT-PERMITTED (manifest violation)".to_string(),
        }
    }
}

/// One `(lane, cell)` row of the minted grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridRow {
    pub lane: Lane,
    pub cell: CellId,
    pub status: CellStatus,
}

/// The minted conformance grid: the certifiable machine artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridVerdict {
    /// FAIL-CLOSED: `true` only when EVERY in-domain `(lane, cell)` is
    /// certifiable. An empty or partial evidence set yields `false`.
    pub certified: bool,
    /// Every in-domain `(lane, cell)`, in manifest order, with its status.
    pub rows: Vec<GridRow>,
}

impl GridVerdict {
    /// The rows that block certification (empty iff `certified`).
    pub fn uncertifiable(&self) -> Vec<&GridRow> {
        self.rows
            .iter()
            .filter(|r| !r.status.is_certifiable())
            .collect()
    }

    /// A deterministic, human- and machine-readable rendering: a top-line
    /// verdict plus one line of proof-of-run per `(lane, cell)`. Stable across
    /// readers of the same inputs (rows are in manifest order).
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(if self.certified {
            "CONFORMANCE GRID — CERTIFIED\n"
        } else {
            "CONFORMANCE GRID — INVALID (not certifiable — see uncertifiable rows below)\n"
        });
        out.push_str(&format!(
            "manifest cells: {}  certifiable: {}  blocking: {}\n\n",
            self.rows.len(),
            self.rows
                .iter()
                .filter(|r| r.status.is_certifiable())
                .count(),
            self.uncertifiable().len(),
        ));
        for r in &self.rows {
            // 20 = the longest lane id (`claude-code/mux-pane`). A lane id is
            // `<harness>/<hosting>` now, and the old 10-column field was sized
            // for the bare provider ids this axis used to carry — every id it
            // cannot fit pushes the cell column out and the grid stops lining up.
            out.push_str(&format!(
                "  {:<20} {:<28} {}\n",
                r.lane.id(),
                r.cell.0,
                r.status.label(),
            ));
        }
        out
    }
}

/// The best (most-satisfying) resolution for a cell across every assembled
/// artifact. A Pass anywhere in the multi-box assembly satisfies the cell; a
/// declared N/A or a valid coverage record satisfies where the manifest allows
/// it; otherwise the presence of a Fail or a Blocked is reported (in that
/// severity order), and total absence is `Missing`.
fn classify(battery: &Battery, lane: Lane, cell: &CellId, artifacts: &[RunArtifact]) -> CellStatus {
    let app = battery.applicability(lane, cell).clone();
    let key = RunArtifact::cell_key(lane, cell);

    let mut saw_fail: Option<String> = None;
    let mut saw_blocked: Option<String> = None;
    let mut saw_na = false;
    let mut saw_coverage = false;
    let mut saw_not_permitted = false;
    let mut saw_any = false;

    for art in artifacts {
        let Some(resolution) = art.grid.get(&key) else {
            continue;
        };
        saw_any = true;
        // A resolution the manifest does not permit for this cell is a manifest
        // violation — never silently upgraded to a satisfying status.
        if !battery.permits(lane, cell, resolution) {
            saw_not_permitted = true;
            continue;
        }
        match resolution {
            CellResolution::Observed(obs) => match &obs.outcome {
                // A real Pass anywhere satisfies immediately — proof-of-run is
                // structural (unrepresentable without a commissioned Runner).
                Outcome::Pass(_) => return CellStatus::Passed,
                Outcome::NotApplicable { .. } => saw_na = true,
                Outcome::Fail { detail, .. } => saw_fail = Some(detail.clone()),
                Outcome::Blocked { reason, .. } => saw_blocked = Some(reason.clone()),
            },
            CellResolution::Coverage(_) => saw_coverage = true,
        }
    }

    // No Pass was found; pick the highest-priority satisfying-or-blocking state.
    if saw_na && matches!(app, Applicability::NaPermitted { .. }) {
        return CellStatus::NotApplicableDeclared;
    }
    if saw_coverage && matches!(app, Applicability::CoveragePermitted) {
        return CellStatus::CoveredElsewhere;
    }
    if let Some(detail) = saw_fail {
        return CellStatus::Failed(detail);
    }
    if let Some(reason) = saw_blocked {
        return CellStatus::Blocked(reason);
    }
    if saw_not_permitted {
        return CellStatus::NotPermitted;
    }
    // A resolution was present but is none of the above (e.g. an N/A or coverage
    // where the manifest does not permit it) — not certifiable.
    if saw_any {
        return CellStatus::NotPermitted;
    }
    CellStatus::Missing
}

/// Mint the conformance grid from the assembled evidence, FAIL-CLOSED.
///
/// For every in-domain `(lane, cell)` in the manifest (via
/// [`Battery::applicable_domain`]), classify its best resolution across ALL
/// `artifacts` (multi-box assembly). The grid is `certified` iff every in-domain
/// cell is certifiable. An empty `artifacts` slice — or one missing any required
/// cell, or one where a required cell only ever resolved Blocked/Fail — mints
/// INVALID: there is no green from nothing.
pub fn mint_grid(battery: &Battery, artifacts: &[RunArtifact]) -> GridVerdict {
    let mut rows = Vec::new();
    for (lane, cell) in battery.applicable_domain() {
        let status = classify(battery, lane, &cell, artifacts);
        rows.push(GridRow { lane, cell, status });
    }
    let certified = rows.iter().all(|r| r.status.is_certifiable());
    GridVerdict { certified, rows }
}

/// The shared evidence sink: serialize a [`RunArtifact`] into `dir` as
/// `<run-id>.artifact.json`. The live battery drivers
/// (`conformance_c1_live` / `conformance_c2_live`) deposit their built
/// artifacts here so the minting invoker can aggregate them; C-5's
/// commissioning run deposits its measured artifacts the same way. Production
/// code (not test-only) so the persist path is exercised and unit-tested, never
/// dead-code-as-enforcement.
pub fn persist_artifact(dir: &Path, artifact: &RunArtifact) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Sanitize the run id for a filename (run ids are short ASCII, but be safe).
    let stem: String = artifact
        .header
        .tuple
        .run
        .0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{stem}.artifact.json"));
    let json = serde_json::to_string_pretty(artifact)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

/// Read every `*.artifact.json` under `dir` into [`RunArtifact`]s. A malformed
/// artifact file is a LOUD error (fail-closed: a corrupt deposit must not be
/// silently skipped — the invoker treats a read error as an uncertifiable
/// grid), never quietly dropped. A missing dir yields an empty vec (a box where
/// nothing was deposited → the minting gate mints INVALID on the empty set).
pub fn read_run_dir(dir: &Path) -> std::io::Result<Vec<RunArtifact>> {
    let mut arts = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(arts),
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".artifact.json"))
        .collect();
    paths.sort(); // deterministic aggregation order
    for p in paths {
        let text = std::fs::read_to_string(&p)?;
        let art: RunArtifact = serde_json::from_str(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed conformance artifact {}: {e}", p.display()),
            )
        })?;
        arts.push(art);
    }
    Ok(arts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::evidence::{CommissioningHeader, RunArtifactBuilder, RunMode};
    use crate::conformance::ids::{BoxId, LaneScope, RunId, RunKind};
    use crate::conformance::journal::AuthorityJournal;
    use crate::conformance::registry::conformance_battery;

    const COMMIT: &str = "commit-a-grid";
    const BOX: &str = "box-lima";
    const TS: &str = "2026-07-15T00:00:00Z";

    /// Build a complete artifact for one lane: every in-domain cell of `lane`
    /// resolves to `Pass`, except any `(cell -> outcome)` override supplied.
    /// Cells the manifest marks n/a-permitted resolve to a declared N/A so the
    /// artifact is a well-formed, complete, honest grid.
    fn lane_artifact(
        journal: &mut AuthorityJournal,
        lane: Lane,
        run: &str,
        overrides: &[(&str, Outcome)],
    ) -> RunArtifact {
        let battery = conformance_battery();
        let tuple = journal
            .commission_run(
                RunId(run.into()),
                LaneScope::one(lane),
                BoxId(BOX.into()),
                COMMIT,
                battery.manifest_digest(),
                battery.aggregation_version.clone(),
                RunKind::Evidence,
                "coord-x (aaaa0000)",
                &mut || format!("tok-{run}"),
            )
            .unwrap();
        let nonce = journal
            .start_run_at(&tuple.run, "runner-x (bbbb1111)", TS, &mut || {
                format!("nonce-{run}")
            })
            .unwrap();
        let header = CommissioningHeader::new(tuple, nonce);
        let mut b = RunArtifactBuilder::new(header, TS);
        let mut applicable = Vec::new();
        for cell in &battery.cells {
            let app = battery.applicability(lane, &cell.id);
            if !app.in_domain() {
                continue;
            }
            applicable.push((lane, cell.id.clone()));
            if let Some((_, outcome)) = overrides.iter().find(|(c, _)| *c == cell.id.0) {
                b.observe(lane, cell.id.clone(), RunMode::Automated, outcome.clone());
                continue;
            }
            let outcome = match app {
                Applicability::NaPermitted { reason } => Outcome::not_applicable(reason.clone()),
                _ => b.runner().pass(vec!["cmd".into()], "observed source state"),
            };
            b.observe(lane, cell.id.clone(), RunMode::Automated, outcome);
        }
        b.build(&applicable).unwrap()
    }

    /// A complete, all-Pass artifact for every lane — a fully certifiable corpus.
    fn all_lanes_pass() -> Vec<RunArtifact> {
        let mut journal = AuthorityJournal::new();
        Lane::ALL
            .iter()
            .map(|&lane| lane_artifact(&mut journal, lane, &format!("run-{}", lane.id()), &[]))
            .collect()
    }

    #[test]
    fn complete_all_pass_is_certified() {
        let arts = all_lanes_pass();
        let grid = mint_grid(&conformance_battery(), &arts);
        assert!(
            grid.certified,
            "a complete all-Pass corpus must certify:\n{}",
            grid.render()
        );
        assert!(grid.uncertifiable().is_empty());
    }

    #[test]
    fn empty_evidence_is_invalid() {
        // The core fail-closed property: a box where NOTHING ran cannot certify.
        let grid = mint_grid(&conformance_battery(), &[]);
        assert!(
            !grid.certified,
            "empty evidence must be INVALID (green from nothing):\n{}",
            grid.render()
        );
        assert!(grid
            .rows
            .iter()
            .all(|r| matches!(r.status, CellStatus::Missing)));
        assert!(!grid.rows.is_empty(), "the manifest domain is non-empty");
    }

    #[test]
    fn a_missing_lane_is_invalid() {
        // Drop one whole lane's artifact — that lane's cells are Missing.
        let mut journal = AuthorityJournal::new();
        let arts: Vec<RunArtifact> = Lane::ALL
            .iter()
            .filter(|&&l| l != Lane::Pi)
            .map(|&lane| lane_artifact(&mut journal, lane, &format!("run-{}", lane.id()), &[]))
            .collect();
        let grid = mint_grid(&conformance_battery(), &arts);
        assert!(
            !grid.certified,
            "a missing lane must be INVALID:\n{}",
            grid.render()
        );
        assert!(grid
            .uncertifiable()
            .iter()
            .any(|r| r.lane == Lane::Pi && matches!(r.status, CellStatus::Missing)));
    }

    #[test]
    fn a_blocked_required_cell_is_invalid() {
        // Simulate a skipped live plane: one required cell resolves Blocked
        // (blocked_gate_off) instead of Pass. This is exactly what the keyed
        // conformance gate deposits when a plane's env var is unset.
        let battery = conformance_battery();
        let target = battery
            .applicable_domain()
            .into_iter()
            .find(|(l, c)| {
                *l == Lane::Pi && matches!(battery.applicability(*l, c), Applicability::Required)
            })
            .expect("Pi has at least one Required cell");
        let mut journal = AuthorityJournal::new();
        let mut arts = Vec::new();
        for &lane in &Lane::ALL {
            let ov: Vec<(&str, Outcome)> = if lane == Lane::Pi {
                vec![(
                    target.1 .0.as_str(),
                    Outcome::blocked_gate_off("QD_PI_LIVE", "set QD_PI_LIVE=1"),
                )]
            } else {
                vec![]
            };
            arts.push(lane_artifact(
                &mut journal,
                lane,
                &format!("run-{}", lane.id()),
                &ov,
            ));
        }
        let grid = mint_grid(&battery, &arts);
        assert!(
            !grid.certified,
            "a Blocked required cell must be INVALID:\n{}",
            grid.render()
        );
        assert!(grid
            .uncertifiable()
            .iter()
            .any(|r| r.cell == target.1 && matches!(r.status, CellStatus::Blocked(_))));
    }

    #[test]
    fn a_failed_cell_is_invalid() {
        let battery = conformance_battery();
        let target = battery
            .applicable_domain()
            .into_iter()
            .find(|(l, c)| {
                *l == Lane::Pi && matches!(battery.applicability(*l, c), Applicability::Required)
            })
            .expect("Pi has a Required cell");
        let mut journal = AuthorityJournal::new();
        // Build the target Pi artifact first so we can mint a Fail against its runner.
        let mut arts = Vec::new();
        for &lane in &Lane::ALL {
            if lane == Lane::Pi {
                continue;
            }
            arts.push(lane_artifact(
                &mut journal,
                lane,
                &format!("run-{}", lane.id()),
                &[],
            ));
        }
        // Pi lane with one Fail (minted against the lane's own runner inside the helper path
        // would need the runner; instead build the Pi artifact inline).
        let tuple = journal
            .commission_run(
                RunId("run-pi".into()),
                LaneScope::one(Lane::Pi),
                BoxId(BOX.into()),
                COMMIT,
                battery.manifest_digest(),
                battery.aggregation_version.clone(),
                RunKind::Evidence,
                "coord-x (aaaa0000)",
                &mut || "tok-pi".to_string(),
            )
            .unwrap();
        let nonce = journal
            .start_run_at(&tuple.run, "runner-x (bbbb1111)", TS, &mut || {
                "nonce-pi".to_string()
            })
            .unwrap();
        let header = CommissioningHeader::new(tuple, nonce);
        let mut b = RunArtifactBuilder::new(header, TS);
        let mut applicable = Vec::new();
        for cell in &battery.cells {
            let app = battery.applicability(Lane::Pi, &cell.id);
            if !app.in_domain() {
                continue;
            }
            applicable.push((Lane::Pi, cell.id.clone()));
            let outcome = if cell.id == target.1 {
                let proof = b.runner().proof(vec!["cmd".into()], "observed a red");
                Outcome::Fail {
                    proof,
                    detail: "the claim did not hold".into(),
                }
            } else if let Applicability::NaPermitted { reason } = app {
                Outcome::not_applicable(reason.clone())
            } else {
                b.runner().pass(vec!["cmd".into()], "observed source state")
            };
            b.observe(Lane::Pi, cell.id.clone(), RunMode::Automated, outcome);
        }
        arts.push(b.build(&applicable).unwrap());
        let grid = mint_grid(&battery, &arts);
        assert!(
            !grid.certified,
            "a Failed cell must be INVALID:\n{}",
            grid.render()
        );
        assert!(grid
            .uncertifiable()
            .iter()
            .any(|r| r.cell == target.1 && matches!(r.status, CellStatus::Failed(_))));
    }

    #[test]
    fn persist_then_read_roundtrips_and_certifies() {
        // Proves the persist -> read -> mint path on REAL artifacts (not dead code):
        // a complete all-Pass corpus survives serialize/deserialize and certifies.
        let dir = std::env::temp_dir().join("qd-c4-grid-persist-certify");
        let _ = std::fs::remove_dir_all(&dir);
        for art in all_lanes_pass() {
            persist_artifact(&dir, &art).unwrap();
        }
        let read_back = read_run_dir(&dir).unwrap();
        assert_eq!(
            read_back.len(),
            Lane::ALL.len(),
            "one artifact per lane round-trips"
        );
        let grid = mint_grid(&conformance_battery(), &read_back);
        assert!(
            grid.certified,
            "a persisted all-Pass corpus must round-trip and certify:\n{}",
            grid.render()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_missing_dir_mints_invalid() {
        // The offline-real-battery shape: a box where nothing was deposited (the
        // live planes never ran) reads as an empty set and mints INVALID.
        let dir = std::env::temp_dir().join("qd-c4-grid-nonexistent-run");
        let _ = std::fs::remove_dir_all(&dir);
        let read_back = read_run_dir(&dir).unwrap();
        assert!(
            read_back.is_empty(),
            "a missing run dir yields no artifacts"
        );
        let grid = mint_grid(&conformance_battery(), &read_back);
        assert!(
            !grid.certified,
            "an empty run dir must mint INVALID:\n{}",
            grid.render()
        );
        assert!(grid
            .rows
            .iter()
            .all(|r| matches!(r.status, CellStatus::Missing)));
    }
}
