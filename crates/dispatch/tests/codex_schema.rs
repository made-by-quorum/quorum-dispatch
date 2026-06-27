//! Codex P2 W1 — schema fixture-diff harness (codex-p2-spec section 5).
//!
//! The committed fixture (tests/fixtures/codex-schema/) is the
//! `codex app-server generate-json-schema` dump of the PINNED codex version
//! (VERSION.pin = the spike/probe binary). Three lanes:
//!
//!   1. ALWAYS-ON fixture integrity: pin present + semver-shaped; both
//!      protocol rollups parse as JSON; every bound-surface manifest name
//!      appears (quoted) in its file.
//!   2. ALWAYS-ON mutation evidence (CR-3, committed re-runnable): the
//!      manifest checker reports a name scrubbed from a doctored copy, and
//!      scripts/codex-schema-diff.sh (compare-only mode) exits nonzero on a
//!      doctored tree + 0 on an identical tree — proving the harness CAN
//!      fail (the spec section 16(d) refutation target).
//!   3. ENV-GATED live lane (QD_CODEX_LIVE=1): full-mode script run —
//!      version-pin check + jailed regenerate from the installed binary +
//!      diff. Drift or version-drift = red, BY NAME.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-schema")
}

fn diff_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/codex-schema-diff.sh")
}

/// Parse manifest.txt lines (`<name> <relative-file>`, `#` comments) and
/// return the names whose QUOTED form (`"name"`) is absent from their file's
/// content. `read` is injected so the mutation-evidence test can feed a
/// doctored copy through the SAME checker the always-on test uses.
fn missing_manifest_names(manifest: &str, read: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut missing = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, file)) = line.split_once(' ') else {
            missing.push(format!("<malformed manifest line: {line}>"));
            continue;
        };
        let needle = format!("\"{name}\"");
        match read(file.trim()) {
            Some(content) if content.contains(&needle) => {}
            _ => missing.push(name.to_string()),
        }
    }
    missing
}

fn read_fixture_file(rel: &str) -> Option<String> {
    fs::read_to_string(fixture_dir().join(rel)).ok()
}

// --- Lane 1: fixture integrity (always-on) ---

#[test]
fn version_pin_present_and_semver_shaped() {
    let pin = fs::read_to_string(fixture_dir().join("VERSION.pin"))
        .expect("VERSION.pin must exist (spec section 3.4: single pin source)");
    let pin = pin.trim();
    let parts: Vec<&str> = pin.split('.').collect();
    assert_eq!(parts.len(), 3, "pin must be MAJOR.MINOR.PATCH, got {pin:?}");
    for p in parts {
        p.parse::<u64>()
            .unwrap_or_else(|_| panic!("non-numeric pin component {p:?} in {pin:?}"));
    }
}

#[test]
fn protocol_rollups_parse_as_json() {
    for rel in [
        "codex_app_server_protocol.schemas.json",
        "codex_app_server_protocol.v2.schemas.json",
    ] {
        let text =
            read_fixture_file(rel).unwrap_or_else(|| panic!("fixture rollup missing: {rel}"));
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&text);
        assert!(
            parsed.is_ok(),
            "{rel} is not valid JSON: {:?}",
            parsed.err()
        );
    }
}

#[test]
fn manifest_names_all_present_in_fixture() {
    let manifest = read_fixture_file("manifest.txt").expect("manifest.txt must exist");
    let missing = missing_manifest_names(&manifest, &read_fixture_file);
    assert!(
        missing.is_empty(),
        "bound-surface names missing from the schema fixture (a bound message \
         vanished — re-pin ceremony required, spec section 5): {missing:?}"
    );
}

// --- Lane 2: mutation evidence (always-on, CR-3) ---

// MUTATION EVIDENCE: a bound message dropped from the schema MUST red lane 1.
// Proven by scrubbing one manifest name from an in-memory copy and asserting
// the SAME checker reports it missing.
#[test]
fn manifest_checker_reds_on_scrubbed_name() {
    let manifest = read_fixture_file("manifest.txt").expect("manifest.txt must exist");
    let scrub = "turn/steer"; // a name lane 1 proves present
    let doctored = |rel: &str| -> Option<String> {
        read_fixture_file(rel).map(|c| c.replace(&format!("\"{scrub}\""), "\"SCRUBBED\""))
    };
    let missing = missing_manifest_names(&manifest, &doctored);
    assert!(
        missing.contains(&scrub.to_string()),
        "checker failed to detect a scrubbed bound name — the manifest test \
         cannot fail (spec section 16(d) refutation realized): {missing:?}"
    );
}

/// Build two small sibling trees from real fixture files for the differ tests.
fn differ_trees(doctor: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let fix = tmp.path().join("fixture");
    let regen = tmp.path().join("regen");
    fs::create_dir_all(&fix).unwrap();
    fs::create_dir_all(&regen).unwrap();
    // VERSION.pin/manifest.txt are qd-side metadata the differ must IGNORE —
    // present in fixture only, proving the -x exclusions work.
    fs::write(fix.join("VERSION.pin"), "0.0.0\n").unwrap();
    fs::write(fix.join("manifest.txt"), "# meta\n").unwrap();
    let sample = read_fixture_file("v1/InitializeParams.json").expect("v1 sample");
    fs::write(fix.join("InitializeParams.json"), &sample).unwrap();
    let regen_sample = if doctor {
        sample.replace('{', "[") // structural corruption
    } else {
        sample.clone()
    };
    fs::write(regen.join("InitializeParams.json"), regen_sample).unwrap();
    (tmp, fix, regen)
}

fn run_differ(fix: &Path, regen: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(diff_script())
        .arg("--regen-dir")
        .arg(regen)
        .env("QD_CODEX_SCHEMA_FIXTURE", fix)
        .output()
        .expect("codex-schema-diff.sh must be runnable")
}

// MUTATION EVIDENCE: the differ MUST exit nonzero on any content delta.
#[test]
fn diff_script_reds_on_doctored_tree() {
    let (_tmp, fix, regen) = differ_trees(true);
    let out = run_differ(&fix, &regen);
    assert_eq!(
        out.status.code(),
        Some(1),
        "differ must exit 1 on drift; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("SCHEMA DRIFT") && stdout.contains("InitializeParams.json"),
        "drift report must name the drifted file: {stdout}"
    );
}

// The green twin that makes the red above meaningful (and pins the
// VERSION.pin/manifest.txt exclusions: fixture-only metadata is not drift).
#[test]
fn diff_script_green_on_identical_tree() {
    let (_tmp, fix, regen) = differ_trees(false);
    let out = run_differ(&fix, &regen);
    assert_eq!(
        out.status.code(),
        Some(0),
        "differ must exit 0 on identical trees; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// --- Lane 3: live regenerate-and-diff (env-gated, jailed) ---

#[test]
fn live_schema_diff_against_installed_binary() {
    if std::env::var("QD_CODEX_LIVE").as_deref() != Ok("1") {
        return; // live lane is opt-in (rule 9: only the jailed lane runs it)
    }
    let out = Command::new("bash")
        .arg(diff_script())
        .output()
        .expect("codex-schema-diff.sh must be runnable");
    assert_eq!(
        out.status.code(),
        Some(0),
        "live schema diff failed (drift or version drift — see report): stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
