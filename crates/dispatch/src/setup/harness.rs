//! Proactive harness detection (C2's substrate, which C4 says does not exist
//! yet): which of **Claude Code**, **codex**, **pi** and **opencode** the human
//! actually has, at what version, and whether qd's wiring for it is in place.
//!
//! # Why this is new code and not a lookup
//!
//! C4: "There is no proactive harness detection at all. Everything is
//! discovered at spawn time, when it fails." qd knows a great deal about each
//! harness — but only from inside the launch path, where the answer arrives too
//! late to help someone who just ran `brew install`. This module asks the same
//! questions BEFORE anything is launched.
//!
//! # Reuse, not re-derivation
//!
//! The two harnesses that already have pin/drift logic keep it. Codex's
//! `--version` sniff and drift verdict
//! ([`crate::provider::codex::app_server::version`]) and pi's version pin
//! ([`crate::provider::pi::pin`]) live in `quorum-qw`, which this crate already
//! depends on, so [`codex_verdict`] and [`pi_verdict`] CALL INTO them rather
//! than restate the rules. `quorum-qw` is not restructured for this — the
//! reachable API was enough.
//!
//! opencode genuinely has nothing to reuse (C4: "opencode has literally
//! nothing"), and needs nothing: its only live transport is the shared ACP
//! driver bridged to `opencode acp`
//! ([`crate::provider::opencode::acp`]), spawned on demand. So the honest
//! verdict for a present opencode is "found, no wiring required" — stated, not
//! silently omitted.
//!
//! Everything here is PURE. The probes (`command -v`, `<bin> --version`, path
//! stats) run in the bin layer through the `Exec` seam and arrive as
//! [`HarnessFacts`].

use std::path::PathBuf;

use crate::provider::codex::app_server::version as codex_version;
use crate::provider::pi::pin as pi_pin;

/// The four harnesses C2 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessId {
    ClaudeCode,
    Codex,
    Pi,
    Opencode,
}

impl HarnessId {
    /// Every harness, in report order (Claude Code first: it is the one with
    /// load-bearing wiring).
    pub const ALL: &'static [HarnessId] = &[
        HarnessId::ClaudeCode,
        HarnessId::Codex,
        HarnessId::Pi,
        HarnessId::Opencode,
    ];

    /// Stable machine id (`--json`) and check-id suffix. THE single spelling of
    /// each harness name in this crate — [`program`](Self::program) and
    /// [`label`](Self::label) derive from it rather than restate it, so there
    /// is one place to change and one place for the provider-literal gate to
    /// pin (see `provider_gate.rs`).
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "claude",
            HarnessId::Codex => "codex",
            HarnessId::Pi => "pi",
            HarnessId::Opencode => "opencode",
        }
    }

    /// The EXECUTABLE name probed on `PATH`. Identical to
    /// [`as_str`](Self::as_str) for all four today; kept as its own method
    /// because the two are different questions — "what does qd call this
    /// harness" vs "what does the shell call its binary" — and a harness whose
    /// npm package name diverges from its command would split them here.
    pub fn program(self) -> &'static str {
        self.as_str()
    }

    /// Human label. Only Claude Code has a display name that is not its
    /// command.
    pub fn label(self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "Claude Code",
            _ => self.as_str(),
        }
    }

    /// What qd gains from this harness — printed when it is NOT found, so the
    /// "not found" line says what the human is missing rather than just
    /// reporting an absence.
    pub fn offers(self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "relay-native sessions (agent-to-agent messaging)",
            HarnessId::Codex => "codex/app-server sessions",
            HarnessId::Pi => "pi sessions (daemon + interactive lanes)",
            HarnessId::Opencode => "opencode sessions over ACP",
        }
    }
}

/// How a harness was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// Resolves on `PATH`.
    OnPath { path: Option<String> },
    /// NOT on `PATH`, but an install was found at a known location. This is the
    /// C5 case for pi: the usual npm-global prefix is not on `PATH`, so a bare
    /// `pi` misses it and qd needs `QD_PI_BIN` to point at it.
    OffPath { path: String },
    /// Not installed anywhere we look.
    Missing,
}

impl Presence {
    pub fn found(&self) -> bool {
        !matches!(self, Presence::Missing)
    }
    pub fn path(&self) -> Option<&str> {
        match self {
            Presence::OnPath { path } => path.as_deref(),
            Presence::OffPath { path } => Some(path),
            Presence::Missing => None,
        }
    }
}

/// What the probes found for one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessFacts {
    pub id: HarnessId,
    pub presence: Presence,
    /// The version string, when a version probe is cheap and it ran.
    pub version: Option<String>,
    /// Verdict against qd's pin: `Some(true)` matches, `Some(false)` drifted,
    /// `None` when there is no pin to compare against (Claude Code, opencode).
    pub pin_ok: Option<bool>,
    /// One-line explanation of `pin_ok`, empty when there is nothing to say.
    pub pin_note: String,
    /// Is qd's wiring for this harness in place? `None` when the harness needs
    /// none.
    pub wired: Option<bool>,
    /// What to do about `wired == Some(false)`.
    pub wiring_note: String,
}

impl HarnessFacts {
    /// A found-but-nothing-else-known harness.
    pub fn new(id: HarnessId, presence: Presence) -> Self {
        HarnessFacts {
            id,
            presence,
            version: None,
            pin_ok: None,
            pin_note: String::new(),
            wired: None,
            wiring_note: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Version verdicts — thin adapters over the EXISTING pin logic in quorum-qw.
// ---------------------------------------------------------------------------

/// Turn codex's existing [`codex_version::SniffOutcome`] into
/// `(version, pin_ok, note)`.
///
/// The drift POLICY is codex's, not setup's: under 0.x semver a
/// (major,minor) mismatch is breaking and a patch delta is warn-and-go
/// (`version::verdict`). Setup only renders it. A failed probe is `None` — the
/// harness is installed but would not answer, which is worth saying and is not
/// the same as "drifted".
pub fn codex_verdict(outcome: &codex_version::SniffOutcome) -> (Option<String>, Option<bool>, String) {
    use codex_version::{SniffOutcome, VersionVerdict};
    let fmt = |v: &codex_version::Version| format!("{}.{}.{}", v.major, v.minor, v.patch);
    match outcome {
        SniffOutcome::Verdict(VersionVerdict::Exact) => (
            Some(codex_version::PINNED.to_string()),
            Some(true),
            format!("matches qd's pin ({})", codex_version::PINNED),
        ),
        SniffOutcome::Verdict(VersionVerdict::PatchDrift { found }) => (
            Some(fmt(found)),
            Some(true),
            format!(
                "patch drift from qd's pin ({}) — supported",
                codex_version::PINNED
            ),
        ),
        SniffOutcome::Verdict(VersionVerdict::Breaking { found, pin }) => (
            Some(fmt(found)),
            Some(false),
            format!(
                "BREAKING drift: qd is pinned to {} (0.x — a minor bump is breaking); \
                 set QD_CODEX_UNPINNED=1 to launch anyway",
                fmt(pin)
            ),
        ),
        SniffOutcome::Unparseable { stdout } => (
            None,
            None,
            format!("`codex --version` printed something unrecognised: {:?}", stdout.trim()),
        ),
        SniffOutcome::ExecFailed { detail } => (None, None, detail.clone()),
    }
}

/// Turn a raw `pi --version` stdout into `(version, pin_ok, note)`, using pi's
/// OWN pin ([`pi_pin::version_matches`] against [`pi_pin::PINNED_VERSION`]).
///
/// pi's protocol is unversioned on the wire and its command surface moved
/// 19→29 across 0.x minors, so an off-pin pi is a real hazard, not a nit —
/// hence a hard `Some(false)` rather than a shrug.
pub fn pi_verdict(version_output: &str) -> (Option<String>, Option<bool>, String) {
    let trimmed = version_output.trim();
    if trimmed.is_empty() {
        return (None, None, "`pi --version` produced no output".to_string());
    }
    let found = trimmed
        .split_whitespace()
        .last()
        .unwrap_or(trimmed)
        .to_string();
    if pi_pin::version_matches(trimmed) {
        (
            Some(found),
            Some(true),
            format!("matches qd's pin ({})", pi_pin::PINNED_VERSION),
        )
    } else {
        (
            Some(found),
            Some(false),
            format!(
                "qd is pinned to pi {} ({}); pi's RPC surface moves between 0.x minors",
                pi_pin::PINNED_VERSION,
                pi_pin::PIN_SPEC
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// C5 — where a pi that is not on PATH actually lives.
// ---------------------------------------------------------------------------

/// The environment variable that points qd at a specific pi binary. Mirrors
/// `quorum_qw::provider::pi::pi_bin`, which reads exactly this and otherwise
/// falls back to a bare `pi` on `PATH`.
pub const PI_BIN_ENV: &str = "QD_PI_BIN";

/// Candidate locations for an npm-global pi, most specific first. PURE: the
/// caller stats them.
///
/// C5: "`QD_PI_BIN` is never set and defaults to a bare `pi` on PATH, which
/// misses the usual npm-global install location." `~/.npm-pi-global/bin/pi` is
/// the location quorum's own provisioning uses (named in
/// `quorum_qw::provider::pi`'s module doc); the rest are the ordinary npm
/// prefixes a human would have.
///
/// Full C5 is out of scope here — setup REPORTS the export rather than setting
/// it, because an env var setup wrote into an rc file would be a second baked
/// fossil of exactly the kind [`crate::shell_init`] ruled against.
pub fn pi_candidates(home: &std::path::Path, npm_prefix: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = npm_prefix.filter(|s| !s.is_empty()) {
        out.push(PathBuf::from(p).join("bin").join("pi"));
    }
    out.push(home.join(".npm-pi-global").join("bin").join("pi"));
    out.push(home.join(".npm-global").join("bin").join("pi"));
    out.push(home.join(".local").join("bin").join("pi"));
    out.push(PathBuf::from("/opt/homebrew/bin/pi"));
    out.push(PathBuf::from("/usr/local/bin/pi"));
    out
}

/// The exact line a human should add for a pi found off `PATH` (C5's
/// deliverable: "report the exact `QD_PI_BIN` export the user needs").
pub fn pi_bin_export(path: &str) -> String {
    format!("export {PI_BIN_ENV}=\"{path}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn every_harness_has_a_distinct_id_and_program() {
        let ids: Vec<_> = HarnessId::ALL.iter().map(|h| h.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate harness id");
        assert_eq!(HarnessId::ALL.len(), 4, "C2 names exactly four harnesses");
        assert_eq!(HarnessId::ClaudeCode.program(), "claude");
    }

    #[test]
    fn presence_answers_found_and_path() {
        assert!(!Presence::Missing.found());
        assert_eq!(Presence::Missing.path(), None);
        let on = Presence::OnPath {
            path: Some("/usr/bin/codex".into()),
        };
        assert!(on.found());
        assert_eq!(on.path(), Some("/usr/bin/codex"));
        let off = Presence::OffPath {
            path: "/h/.npm-pi-global/bin/pi".into(),
        };
        assert!(off.found());
        assert_eq!(off.path(), Some("/h/.npm-pi-global/bin/pi"));
    }

    // --- codex: the verdict comes from qw's own policy ----------------------

    #[test]
    fn codex_exact_and_patch_drift_both_pass() {
        use codex_version::{SniffOutcome, Version, VersionVerdict};
        let (v, ok, note) = codex_verdict(&SniffOutcome::Verdict(VersionVerdict::Exact));
        assert_eq!(v.as_deref(), Some(codex_version::PINNED));
        assert_eq!(ok, Some(true));
        assert!(note.contains("matches"), "{note}");

        let found = Version {
            major: 0,
            minor: 146,
            patch: 9,
        };
        let (v, ok, note) = codex_verdict(&SniffOutcome::Verdict(VersionVerdict::PatchDrift { found }));
        assert_eq!(v.as_deref(), Some("0.146.9"));
        assert_eq!(ok, Some(true), "patch drift is warn-and-go, not a failure");
        assert!(note.contains("patch drift"), "{note}");
    }

    #[test]
    fn codex_breaking_drift_names_the_pin_and_the_override() {
        use codex_version::{SniffOutcome, Version, VersionVerdict};
        let (v, ok, note) = codex_verdict(&SniffOutcome::Verdict(VersionVerdict::Breaking {
            found: Version { major: 0, minor: 200, patch: 0 },
            pin: Version { major: 0, minor: 146, patch: 1 },
        }));
        assert_eq!(v.as_deref(), Some("0.200.0"));
        assert_eq!(ok, Some(false));
        assert!(note.contains("0.146.1"), "{note}");
        assert!(note.contains("QD_CODEX_UNPINNED"), "{note}");
    }

    #[test]
    fn a_codex_that_will_not_answer_is_unknown_not_drifted() {
        use codex_version::SniffOutcome;
        let (v, ok, note) = codex_verdict(&SniffOutcome::ExecFailed {
            detail: "codex --version exited Some(1)".into(),
        });
        assert_eq!(v, None);
        assert_eq!(ok, None, "a failed probe is not a drift verdict");
        assert!(note.contains("exited"), "{note}");

        let (v, ok, _) = codex_verdict(&SniffOutcome::Unparseable {
            stdout: "who knows\n".into(),
        });
        assert_eq!(v, None);
        assert_eq!(ok, None);
    }

    // --- pi -----------------------------------------------------------------

    #[test]
    fn pi_verdict_uses_pis_own_pin() {
        let (v, ok, note) = pi_verdict(pi_pin::PINNED_VERSION);
        assert_eq!(v.as_deref(), Some(pi_pin::PINNED_VERSION));
        assert_eq!(ok, Some(true));
        assert!(note.contains(pi_pin::PINNED_VERSION), "{note}");

        // The `pi 0.80.2`-shaped output pi's own matcher tolerates.
        let (_, ok, _) = pi_verdict(&format!("pi {}", pi_pin::PINNED_VERSION));
        assert_eq!(ok, Some(true));
    }

    #[test]
    fn an_off_pin_pi_is_a_hard_no_with_the_reason() {
        let (v, ok, note) = pi_verdict("0.61.0");
        assert_eq!(v.as_deref(), Some("0.61.0"));
        assert_eq!(ok, Some(false));
        assert!(note.contains("RPC surface"), "{note}");
        assert!(note.contains(pi_pin::PIN_SPEC), "{note}");
    }

    #[test]
    fn an_empty_pi_probe_is_unknown() {
        let (v, ok, _) = pi_verdict("  \n ");
        assert_eq!(v, None);
        assert_eq!(ok, None);
    }

    // --- C5 -----------------------------------------------------------------

    #[test]
    fn pi_candidates_lead_with_the_npm_prefix_then_quorums_own_location() {
        let c = pi_candidates(Path::new("/h"), Some("/opt/npm"));
        assert_eq!(c[0], Path::new("/opt/npm/bin/pi"));
        assert_eq!(c[1], Path::new("/h/.npm-pi-global/bin/pi"));
        assert!(c.contains(&PathBuf::from("/usr/local/bin/pi")));

        // No npm prefix exported: the list still starts somewhere useful.
        let c = pi_candidates(Path::new("/h"), None);
        assert_eq!(c[0], Path::new("/h/.npm-pi-global/bin/pi"));
        // An empty prefix is treated as unset, not as "/bin/pi".
        assert_eq!(pi_candidates(Path::new("/h"), Some(""))[0], c[0]);
    }

    #[test]
    fn the_reported_export_is_copy_pasteable() {
        assert_eq!(
            pi_bin_export("/h/.npm-pi-global/bin/pi"),
            r#"export QD_PI_BIN="/h/.npm-pi-global/bin/pi""#
        );
    }

    #[test]
    fn the_env_var_matches_what_the_launch_path_reads() {
        // If these ever diverge, setup would report an export qd ignores.
        assert_eq!(PI_BIN_ENV, "QD_PI_BIN");
    }
}
