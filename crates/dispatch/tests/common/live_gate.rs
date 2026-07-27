//! C-4 (a) — the single-home keyed live gate. Pure `std`, NO deps, so any test
//! binary can include it via `#[path = "common/live_gate.rs"] mod live_gate;`
//! without pulling the heavier `common` fixture helpers (disk/compile footprint).
//!
//! THE only place a `QD_*_LIVE` plane env var may be read — enforced by the
//! drift lint `tests/conformance_gate_singlehome.rs`, which reds if any other
//! file under `tests/` reads a `QD_*_LIVE` var directly. So the set of live
//! planes is DERIVED from the routed calls, never a hand-list that drifts.
#![allow(dead_code)]

/// The keyed live gate. Returns `true` iff `plane_env` is enabled (`== "1"`);
/// on a disabled plane it logs an HONEST skip (never a silent green) and returns
/// `false`. Callers pass the plane var NAME (a literal) and a short cell label;
/// the env READ happens only here.
///
/// Fail-closed safety does NOT live here and needs no flag: a skipped
/// MANIFEST-cell driver (`conformance_c1_live` / `conformance_c2_live`) thereby
/// deposits no `Outcome::Pass` for its cells, so the minting gate
/// (`dispatch::conformance::grid::mint_grid`) mints INVALID for those cells
/// (absence-of-proof → INVALID by construction). This gate only routes the read
/// and refuses to let a skip pass as green.
pub fn conformance_gate(plane_env: &str, cell_label: &str) -> bool {
    if std::env::var(plane_env).as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "SKIP {cell_label}: {plane_env} != 1 (live plane off — recorded as a NON-RUN, never a pass)"
    );
    false
}

/// Like [`conformance_gate`] but accepting the broader truthy spellings the two
/// manifest-cell drivers use (`"1" | "true" | "yes"`), preserving their existing
/// semantics while routing the read through the single home.
pub fn conformance_gate_truthy(plane_env: &str, cell_label: &str) -> bool {
    if matches!(std::env::var(plane_env).as_deref(), Ok("1") | Ok("true") | Ok("yes")) {
        return true;
    }
    eprintln!(
        "SKIP {cell_label}: {plane_env} not truthy (live plane off — recorded as a NON-RUN, never a pass)"
    );
    false
}
