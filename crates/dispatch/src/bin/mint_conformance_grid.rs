//! C-4 (a) — the conformance grid minting invoker (the fail-closed CERTIFY entry).
//!
//! Reads a run dir of deposited `RunArtifact`s (written by the live battery
//! drivers `conformance_c1_live` / `conformance_c2_live` when
//! `QD_CONFORMANCE_RUNDIR` is set, and by C-5's measured commissioning run),
//! mints the fail-closed grid via `mint_grid`, prints it, and exits NON-ZERO
//! unless every manifest cell has a real `Pass`. A green grid is mintable ONLY
//! from real proof-of-run for every manifest cell — a box where the live planes
//! never ran deposits nothing (or only `Blocked`), so this mints INVALID. There
//! is no flag that turns the check off; safety is absence-of-proof → INVALID.
//!
//! Usage: `mint-conformance-grid <run-dir>`  (or set `QD_CONFORMANCE_RUNDIR`).

use std::path::PathBuf;
use std::process::ExitCode;

use dispatch::conformance::registry::conformance_battery;
use dispatch::conformance::{mint_grid, read_run_dir};

fn main() -> ExitCode {
    let dir = match std::env::args()
        .nth(1)
        .or_else(|| std::env::var("QD_CONFORMANCE_RUNDIR").ok())
    {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!(
                "usage: mint-conformance-grid <run-dir>   (or set QD_CONFORMANCE_RUNDIR)"
            );
            return ExitCode::from(2);
        }
    };

    let artifacts = match read_run_dir(&dir) {
        Ok(a) => a,
        Err(e) => {
            // Fail-closed: a corrupt/unreadable deposit is not a certifiable grid.
            println!("CONFORMANCE GRID — INVALID (evidence read error under {dir:?}: {e})");
            return ExitCode::FAILURE;
        }
    };

    let grid = mint_grid(&conformance_battery(), &artifacts);
    print!("{}", grid.render());
    println!(
        "\nrun dir: {dir:?}   artifacts read: {}",
        artifacts.len()
    );
    if grid.certified {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
