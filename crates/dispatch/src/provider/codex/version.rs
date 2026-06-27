//! Codex version pin + drift policy (codex-p2-spec section 3.4).
//!
//! Single source of truth for the pinned version: the W1 fixture
//! `tests/fixtures/codex-schema/VERSION.pin` (the spike/probe binary,
//! `0.134.0`), `include_str!`'d so the pin lives in EXACTLY one place — the
//! schema harness and this drift check read the same byte string.
//!
//! This module is PURE: it sniffs `codex --version`, parses the version, and
//! computes a [`VersionVerdict`]. It owns NO env-override logic — the launch
//! path (W4) maps `QD_CODEX_UNPINNED=1` onto a verdict; keeping that out of here
//! keeps the verdict a deterministic function of (found, pin).
//!
//! 0.x semver (codex is pre-1.0): a MINOR bump is breaking, so a (major,minor)
//! mismatch is [`Breaking`](VersionVerdict::Breaking); a patch-only delta is
//! [`PatchDrift`](VersionVerdict::PatchDrift).

use crate::exec::Exec;

/// The pinned codex version, trimmed from the single W1 pin file. The relative
/// path is from THIS module: src/provider/codex/version.rs →
/// ../../../tests/fixtures/codex-schema/VERSION.pin (src → crate root, then into
/// tests/). Verified to compile against the committed fixture.
pub const PINNED: &str = include_pin();

/// `include_str!` + trim at const-eval time so callers always see the trimmed
/// pin (the file ends with a newline).
const fn include_pin() -> &'static str {
    let raw = include_str!("../../../tests/fixtures/codex-schema/VERSION.pin");
    trim_ascii_end(raw)
}

/// const-compatible trailing-whitespace trim (so `PINNED` is `"0.134.0"`, not
/// `"0.134.0\n"`). `str::trim` is not const on this toolchain.
const fn trim_ascii_end(s: &str) -> &str {
    let mut bytes = s.as_bytes();
    while let [rest @ .., last] = bytes {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    // SAFETY: trimming whole trailing ASCII bytes off a valid UTF-8 string keeps
    // it valid UTF-8 (ASCII whitespace bytes are never UTF-8 continuation bytes).
    match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => panic!("VERSION.pin is not valid UTF-8"),
    }
}

/// A parsed `(major, minor, patch)` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse `"MAJOR.MINOR.PATCH"` (exactly three numeric components). Returns
    /// `None` on any shape that is not three dotted integers.
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.trim().split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None; // more than 3 components is not a pinned-shape version
        }
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

/// The drift verdict (codex-p2-spec section 3.4). `Exact` and `PatchDrift`
/// proceed (the latter with a warning); `Breaking` is the loud named error the
/// launch path raises unless overridden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionVerdict {
    /// Found == pin exactly.
    Exact,
    /// Patch-only delta (same major+minor). Warn-and-go.
    PatchDrift { found: Version },
    /// (major, minor) mismatch — breaking under 0.x semver (minor counts as
    /// major). The launch path raises a named error (re-pin via the W1 harness)
    /// unless `QD_CODEX_UNPINNED=1`.
    Breaking { found: Version, pin: Version },
}

/// The pure verdict function (codex-p2-spec section 3.4). 0.x rule: a
/// (major,minor) mismatch is Breaking; equal (major,minor) with a different
/// patch is PatchDrift; fully equal is Exact.
pub fn verdict(found: Version, pin: Version) -> VersionVerdict {
    if found.major != pin.major || found.minor != pin.minor {
        return VersionVerdict::Breaking { found, pin };
    }
    if found.patch != pin.patch {
        return VersionVerdict::PatchDrift { found };
    }
    VersionVerdict::Exact
}

/// The outcome of sniffing `codex --version` (codex-p2-spec section 3.4): the
/// found version + its verdict against [`PINNED`], or a parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SniffOutcome {
    /// Parsed a version and computed a verdict against the pin.
    Verdict(VersionVerdict),
    /// `codex --version` ran but its stdout did not parse as `codex-cli X.Y.Z`.
    Unparseable { stdout: String },
    /// `codex --version` failed to run (nonzero exit / timeout / spawn error).
    ExecFailed { detail: String },
}

/// Parse the `codex --version` stdout. Observed format: `codex-cli 0.134.0`
/// (the second whitespace token is the semver). Permissive: scans tokens for
/// the first that parses as a 3-part version, so a `codex-cli 0.134.0 (extra)`
/// line still yields the version.
pub fn parse_version_stdout(stdout: &str) -> Option<Version> {
    stdout.split_whitespace().find_map(Version::parse)
}

/// Sniff the installed codex version via the [`Exec`] seam (one-shot
/// `codex --version`) and compute its [`SniffOutcome`] against [`PINNED`].
/// Pure with respect to the injected exec — no real spawn in tests.
pub fn sniff(exec: &dyn Exec) -> SniffOutcome {
    let pin = match Version::parse(PINNED) {
        Some(p) => p,
        // PINNED comes from a tested fixture (codex_schema.rs asserts it is
        // semver-shaped), so this is unreachable in practice; degrade loudly
        // rather than panic.
        None => {
            return SniffOutcome::ExecFailed {
                detail: format!("internal: pinned version {PINNED:?} is not semver-shaped"),
            }
        }
    };
    let out = match exec.run("codex", &["--version".to_string()], &[], None, Some(10_000)) {
        Ok(o) => o,
        Err(e) => {
            return SniffOutcome::ExecFailed {
                detail: format!("codex --version failed to run: {e}"),
            }
        }
    };
    if out.timed_out || out.status != Some(0) {
        return SniffOutcome::ExecFailed {
            detail: format!(
                "codex --version exited {:?} (timed_out={}): {}",
                out.status,
                out.timed_out,
                out.stderr.trim()
            ),
        };
    }
    match parse_version_stdout(&out.stdout) {
        Some(found) => SniffOutcome::Verdict(verdict(found, pin)),
        None => SniffOutcome::Unparseable {
            stdout: out.stdout.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn pinned_is_the_fixture_version() {
        // The single-source pin is exactly the W1 fixture's VERSION.pin, trimmed.
        assert_eq!(PINNED, "0.134.0");
        assert_eq!(Version::parse(PINNED), Some(v(0, 134, 0)));
    }

    #[test]
    fn parse_rejects_non_three_part() {
        assert_eq!(Version::parse("0.134"), None);
        assert_eq!(Version::parse("0.134.0.1"), None);
        assert_eq!(Version::parse("garbage"), None);
        assert_eq!(Version::parse("0.x.0"), None);
    }

    #[test]
    fn parse_version_stdout_extracts_from_codex_cli_line() {
        // The observed format (codex-p2-spec section 3.4).
        assert_eq!(
            parse_version_stdout("codex-cli 0.134.0"),
            Some(v(0, 134, 0))
        );
        // A trailing build suffix on its own token does not block extraction.
        assert_eq!(
            parse_version_stdout("codex-cli 0.140.2 (homebrew)"),
            Some(v(0, 140, 2))
        );
        assert_eq!(parse_version_stdout("no version here"), None);
    }

    // === Verdict truth table (codex-p2-spec section 3.4) ===

    #[test]
    fn verdict_exact() {
        assert_eq!(verdict(v(0, 134, 0), v(0, 134, 0)), VersionVerdict::Exact);
    }

    #[test]
    fn verdict_patch_only_is_patch_drift() {
        assert_eq!(
            verdict(v(0, 134, 5), v(0, 134, 0)),
            VersionVerdict::PatchDrift {
                found: v(0, 134, 5)
            }
        );
        // Patch drift downward is still patch drift.
        assert_eq!(
            verdict(v(0, 134, 0), v(0, 134, 9)),
            VersionVerdict::PatchDrift {
                found: v(0, 134, 0)
            }
        );
    }

    // MUTATION EVIDENCE (codex-p2-spec section 13 "version sniff disabled / drift
    // not loud"): a MINOR drift under 0.x MUST be Breaking, not PatchDrift. If
    // `verdict` collapsed the 0.x minor rule (treating minor like patch), this
    // assertion reds. NAMED: minor_drift_under_0x_is_breaking.
    #[test]
    fn minor_drift_under_0x_is_breaking() {
        assert_eq!(
            verdict(v(0, 140, 0), v(0, 134, 0)),
            VersionVerdict::Breaking {
                found: v(0, 140, 0),
                pin: v(0, 134, 0),
            },
            "0.x minor bump is breaking (spec 3.4) — must NOT be PatchDrift"
        );
    }

    // MUTATION EVIDENCE: a MAJOR drift MUST be Breaking. NAMED.
    #[test]
    fn major_drift_is_breaking() {
        assert_eq!(
            verdict(v(1, 134, 0), v(0, 134, 0)),
            VersionVerdict::Breaking {
                found: v(1, 134, 0),
                pin: v(0, 134, 0),
            }
        );
    }

    // === sniff against a fake Exec ===

    #[test]
    fn sniff_exact_against_pinned_binary() {
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.134.0\n", "");
        assert_eq!(sniff(&exec), SniffOutcome::Verdict(VersionVerdict::Exact));
    }

    // MUTATION EVIDENCE: a drifted (minor) binary sniffed MUST yield Breaking —
    // the loud-error trigger. NAMED: sniff_drifted_binary_is_breaking.
    #[test]
    fn sniff_drifted_binary_is_breaking() {
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.140.0\n", "");
        assert_eq!(
            sniff(&exec),
            SniffOutcome::Verdict(VersionVerdict::Breaking {
                found: v(0, 140, 0),
                pin: v(0, 134, 0),
            }),
            "a drifted codex binary must sniff as Breaking (the launch-path loud error)"
        );
    }

    #[test]
    fn sniff_patch_drift() {
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(0), "codex-cli 0.134.7\n", "");
        assert_eq!(
            sniff(&exec),
            SniffOutcome::Verdict(VersionVerdict::PatchDrift {
                found: v(0, 134, 7)
            })
        );
    }

    #[test]
    fn sniff_unparseable_stdout() {
        let exec = ScriptedExec::new().on("codex", &["--version"], Some(0), "weird output\n", "");
        assert_eq!(
            sniff(&exec),
            SniffOutcome::Unparseable {
                stdout: "weird output\n".to_string()
            }
        );
    }

    #[test]
    fn sniff_exec_nonzero_is_exec_failed() {
        let exec =
            ScriptedExec::new().on("codex", &["--version"], Some(127), "", "command not found");
        match sniff(&exec) {
            SniffOutcome::ExecFailed { detail } => {
                assert!(detail.contains("command not found") || detail.contains("127"));
            }
            other => panic!("expected ExecFailed, got {other:?}"),
        }
    }
}
