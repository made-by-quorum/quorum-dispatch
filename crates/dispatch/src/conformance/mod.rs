//! The qd conformance battery & evidence schema (C-1 of the qd-hardening
//! CONFORMANCE composite). One battery, defined once, with **structural
//! honesty**: the instrument's whole value is that a stranger can trust a green
//! cell, so a cell that did not execute cannot serialize as passed — enforced by
//! the type system, not by a reviewer.
//!
//! Module map:
//! - [`ids`] — identity newtypes; authority-issued values have crate-private
//!   constructors (issued by the registrar, never the producer).
//! - [`evidence`] — the honesty spine: `Outcome` (no `bool pass`), `ProofOfRun`
//!   (mintable only via a commissioned `Runner`), `Gate` (gate-off → blocked by
//!   construction), `BoxCoverageRecord` (outside the outcome union), and the
//!   `RunArtifact` with its structural commissioning header.
//! - [`journal`] — the one append-only authority journal (run + designation
//!   events, one ordinal domain, append-time integrity, terminal states).
//! - [`attribution`] — the typed append-only attribution record + the unique
//!   active head per observation (F-2).
//! - [`registry`] — the D1–D7 dimension registry / manifest (cells +
//!   applicability map + digest + uniformity).
//!
//! **Boundary with C-3 (the tier scheme).** C-1 makes each artifact / record /
//! chain / journal structurally honest and carries every operand the tier
//! function reads; C-3's A10 runs the *corpus-scope* validate-then-compute
//! (v1–v6 over the assembled corpus, designation replay, window selection,
//! pooling, predicates) and computes `tier(...)`. This crate does not compute
//! tiers.

pub mod attribution;
pub mod evidence;
pub mod grid;
pub mod harness;
pub mod ids;
pub mod journal;
pub mod registry;
pub mod tier;
pub mod tier_doc;

#[cfg(test)]
mod tier_tests;

#[cfg(test)]
mod flake_exercise;

#[cfg(test)]
mod governance_exercise;

pub use evidence::{
    BoxCoverageRecord, CellResolution, CommissioningHeader, CommissioningTuple, Gate, Observation,
    Outcome, ProofOfRun, RunArtifact, RunArtifactBuilder, RunMode, Runner,
};
pub use grid::{mint_grid, persist_artifact, read_run_dir, CellStatus, GridRow, GridVerdict};
pub use ids::{
    AggregationVersion, ArtifactDigest, AttributionSeq, BoxId, CellId, CommissionToken,
    JournalOrdinal, Lane, LaneScope, LaunchNonce, ManifestDigest, ObservationId, RecordId, RunId,
    RunKind, RunSequence,
};
