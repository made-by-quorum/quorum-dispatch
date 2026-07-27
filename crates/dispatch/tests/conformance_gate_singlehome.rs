//! C-4 (a) drift-proof single-home lint for the conformance live gate.
//!
//! Modeled on `src/safe_kill.rs`'s `no_raw_group_kill_outside_safe_kill`: a
//! runtime source scan (from disk, over `tests/`) that FAILS naming any offender.
//! The invariant: the ONLY file allowed to read a `QD_*_LIVE` plane env var is
//! the single-home gate `tests/common/live.rs` (`conformance_gate`). Every other
//! live test must route its plane read through that gate, so the set of live
//! planes is DERIVED from the routed calls — never a hand-maintained list that
//! silently drifts when a new (or forgotten) live test forks a raw `env::var`.
//! This is the structural answer to the manifest-drift hazard: a new live test
//! that reads its plane directly REDS this lint until it routes through the gate.
//!
//! Robust to the honest env-read spellings in the tree (`env::var`,
//! `std::env::var`, `var_os`, and any wrapper whose call contains `var`); it does
//! not (and per the cooperative-agents trust model need not) defend against
//! dynamic string-building obfuscation.

use std::path::{Path, PathBuf};

/// Recursively collect every `.rs` file under `dir`.
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Conformance lint: NO `QD_*_LIVE` plane env read may exist under `tests/`
/// outside the single-home gate `tests/common/live.rs`. Scans from disk at
/// runtime and FAILS naming every offender (with the route-through fix).
#[test]
fn no_qd_live_env_read_outside_conformance_gate() {
    let tests_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    // The one file allowed to hold a raw plane env read.
    let home = tests_root.join("common").join("live_gate.rs");

    // Tokens assembled via `concat!` so THIS file's matching logic does not
    // self-match on the pattern strings (the contiguous substrings never appear
    // in this source); the doc/message lines are excluded by skipping this file.
    let tok_prefix = concat!("QD", "_");
    let tok_live = concat!("_", "LIVE");
    let tok_var = concat!("va", "r");

    let mut files = Vec::new();
    rs_files(&tests_root, &mut files);

    let mut offenders = Vec::new();
    for path in files {
        if path == home {
            continue;
        }
        // Skip this lint's own source (its doc + error strings name the tokens).
        if path.file_name().and_then(|n| n.to_str()) == Some("conformance_gate_singlehome.rs") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (lineno, line) in text.lines().enumerate() {
            // An HONEST env read of a QD_*_LIVE plane: the line names the var
            // literal AND performs an env read (any spelling containing `var`).
            // Comment mentions and `eprintln!` skip messages (no `var`) do not
            // match; `if !live()` call sites (no var literal) do not match.
            if line.contains(tok_prefix) && line.contains(tok_live) && line.contains(tok_var) {
                offenders.push(format!("{}:{}: {}", path.display(), lineno + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a QD_*_LIVE plane env read was found OUTSIDE the single-home gate \
         tests/common/live.rs — route it through `common::live::conformance_gate(plane, cell)` \
         so the live-plane set stays derived, not hand-listed:\n{}",
        offenders.join("\n")
    );
}
