//! The verdict vocabulary `qd setup` speaks: one [`Check`] per thing setup
//! knows how to look at, an optional [`Remedy`] saying how to repair it, and
//! the [`SetupReport`] that renders them and rules the exit code.
//!
//! Ported in SHAPE (not code) from `qrm doctor`'s detect-then-`--fix` report —
//! `qrm` does not ship (punch list "Shipping shape"), so nothing in that crate
//! is reusable, only its design. The one deliberate divergence: `qrm doctor`
//! decides its repairs inline in the live runner, which makes the decision
//! untestable without a machine. Here the DECISION is a pure function that
//! attaches a [`Remedy`] to each check, and the bin layer is the only thing
//! that executes one. Every branch below is therefore unit-testable with no
//! filesystem at all.

use std::path::PathBuf;

/// One check's verdict.
///
/// The split that matters is **`Fail` vs everything else**: `Fail` is the only
/// status that gates the exit code, so it means "something REQUIRED is missing
/// and qd will not work correctly until it is fixed". `Warn` is "wired, but
/// not the way setup would wire it" — worth repairing under `--fix`, never
/// worth failing a script over. `Info` is an FYI that has no repair at all
/// (the C17 plugin line), and `Skip` is "not applicable on this machine".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// In place; nothing to do.
    Ok,
    /// Was not in place; `--fix` repaired it this run.
    Fixed,
    /// Advisory only — no repair exists or none is wanted.
    Info,
    /// Wired, but not correctly/completely. Repaired by `--fix`; does NOT gate
    /// the exit code.
    Warn,
    /// Required and missing. Repaired by `--fix` where a remedy exists; gates
    /// the exit code when it survives the fix pass.
    Fail,
    /// Not applicable here (e.g. the from-source placement check under
    /// Homebrew).
    Skip,
}

impl Status {
    /// Fixed-width glyph for the rendered report (the `qrm doctor` column
    /// layout, which reads well when a dozen checks stack up).
    pub fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "ok   ",
            Status::Fixed => "fixed",
            Status::Info => "fyi  ",
            Status::Warn => "WARN ",
            Status::Fail => "FAIL ",
            Status::Skip => "skip ",
        }
    }

    /// Stable machine-readable name for `--json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Fixed => "fixed",
            Status::Info => "info",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }

    /// Does `--fix` try to apply this check's remedy? `Fail` and `Warn` only —
    /// an `Ok`/`Fixed`/`Info`/`Skip` check has nothing to repair, and applying
    /// a remedy to one would mean writing to a file that is already correct.
    pub fn wants_fix(self) -> bool {
        matches!(self, Status::Fail | Status::Warn)
    }
}

/// What `--fix` would DO for a check, as data rather than as a closure.
///
/// RULE: a remedy names an effect, it never performs one. The bin layer owns
/// every write (mirroring how `bin/qd/verbs/bootstrap.rs` owns the writes for
/// the pure `dispatch::bootstrap` deciders), so `assess` stays a pure function
/// of gathered facts and the whole decision table is testable in-process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remedy {
    /// `mkdir -p` each path (the `~/.quorum` + `bin` + `state` structure).
    CreateDirs(Vec<PathBuf>),
    /// Hand off to `qd bootstrap` for the engine data dir. Setup NEVER
    /// reimplements that step — `qd bootstrap` owns `~/.quorum/dispatch`.
    RunBootstrap,
    /// Copy `names` from `src_dir` to `dst_dir`, mode 0755. FROM-SOURCE ONLY —
    /// see [`crate::setup::layout::InstallChannel`]; a Homebrew install is
    /// check-and-explain, never copy.
    PlaceBinaries {
        src_dir: PathBuf,
        dst_dir: PathBuf,
        names: Vec<String>,
    },
    /// Upsert the managed PATH block in `rc`, exporting `bin_dir`.
    WriteRcBlock { rc: PathBuf, bin_dir: PathBuf },
    /// Write `mcpServers.relay` in `~/.claude.json`, pointing at `command`.
    /// Order-preserving and surgical — see [`crate::setup::relay_pin`].
    WriteRelayPin { path: PathBuf, command: String },
    /// No automated repair: tell the human exactly what to run/do.
    Manual(String),
}

impl Remedy {
    /// One line naming what would happen (or what the human must run). This is
    /// what a non-`--fix` run prints under a failing check, so it has to be
    /// actionable on its own.
    pub fn describe(&self) -> String {
        match self {
            Remedy::CreateDirs(dirs) => format!(
                "create {}",
                dirs.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Remedy::RunBootstrap => "run `qd bootstrap` (creates the engine data dir)".to_string(),
            Remedy::PlaceBinaries {
                src_dir,
                dst_dir,
                names,
            } => format!(
                "copy {} from {} to {}",
                names.join(" + "),
                src_dir.display(),
                dst_dir.display()
            ),
            Remedy::WriteRcBlock { rc, bin_dir } => format!(
                "add a managed PATH block to {} exporting {}",
                rc.display(),
                bin_dir.display()
            ),
            Remedy::WriteRelayPin { path, command } => format!(
                "set mcpServers.relay.command = `{command}` in {}",
                path.display()
            ),
            Remedy::Manual(s) => s.clone(),
        }
    }

    /// Can `--fix` apply this without human help? `Manual` cannot, by
    /// definition — which is why a `Fail` carrying a `Manual` remedy survives
    /// the fix pass and keeps the exit code non-zero. That is the contract:
    /// `qd setup --fix` exits non-zero exactly when it could not finish the job.
    pub fn is_automatic(&self) -> bool {
        !matches!(self, Remedy::Manual(_))
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Stable machine id (`--json` consumers key off this, never off `name`).
    pub id: &'static str,
    /// Human column label.
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub remedy: Option<Remedy>,
}

impl Check {
    pub fn new(id: &'static str, name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Check {
            id,
            name,
            status,
            detail: detail.into(),
            remedy: None,
        }
    }

    pub fn with_remedy(mut self, remedy: Remedy) -> Self {
        self.remedy = Some(remedy);
        self
    }
}

/// The whole verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupReport {
    pub checks: Vec<Check>,
}

impl SetupReport {
    pub fn push(&mut self, check: Check) {
        self.checks.push(check);
    }

    /// EXIT-CODE CONTRACT (stated here because `qd setup` is scriptable):
    /// **0** when every required piece is in place — or was put in place this
    /// run — and **1** when at least one required piece is still missing.
    /// `Warn`/`Info`/`Skip` never gate: a codex that drifted a patch version,
    /// or a harness the human simply does not have, is not a setup failure.
    pub fn exit_code(&self) -> i32 {
        if self.checks.iter().any(|c| c.status == Status::Fail) {
            1
        } else {
            0
        }
    }

    /// Every check `--fix` would act on, in report order.
    pub fn fixable(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.status.wants_fix() && c.remedy.is_some())
            .collect()
    }

    /// Is there anything for `--fix` to actually DO (as opposed to only
    /// `Manual` remedies, which no flag can apply)?
    pub fn has_automatic_fixes(&self) -> bool {
        self.fixable()
            .iter()
            .any(|c| c.remedy.as_ref().is_some_and(|r| r.is_automatic()))
    }

    /// The `[setup]`-prefixed report, one line per check plus the remedy line
    /// under anything that needs one. Prefixed like `qd bootstrap`'s output so
    /// the two read as one first run.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!(
                "[setup] [{}] {:<14} {}\n",
                c.status.glyph(),
                c.name,
                c.detail
            ));
            if c.status.wants_fix() {
                if let Some(r) = &c.remedy {
                    out.push_str(&format!("[setup]                        → {}\n", r.describe()));
                }
            }
        }
        out.push_str(&format!(
            "[setup] {}\n",
            if self.exit_code() == 0 {
                "setup: OK"
            } else {
                "setup: INCOMPLETE — re-run with `qd setup --fix`, or apply the → lines above"
            }
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(status: Status) -> Check {
        Check::new("x", "x", status, "d")
    }

    #[test]
    fn only_fail_gates_the_exit_code() {
        let mut r = SetupReport::default();
        r.push(c(Status::Ok));
        r.push(c(Status::Warn));
        r.push(c(Status::Info));
        r.push(c(Status::Skip));
        r.push(c(Status::Fixed));
        assert_eq!(r.exit_code(), 0, "nothing here is a required-and-missing piece");
        r.push(c(Status::Fail));
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn fix_acts_on_fail_and_warn_only() {
        assert!(Status::Fail.wants_fix());
        assert!(Status::Warn.wants_fix());
        for s in [Status::Ok, Status::Fixed, Status::Info, Status::Skip] {
            assert!(!s.wants_fix(), "{s:?} has nothing to repair");
        }
    }

    #[test]
    fn fixable_needs_both_a_status_and_a_remedy() {
        let mut r = SetupReport::default();
        r.push(c(Status::Fail)); // no remedy attached
        assert!(r.fixable().is_empty());
        r.push(c(Status::Fail).with_remedy(Remedy::RunBootstrap));
        assert_eq!(r.fixable().len(), 1);
    }

    #[test]
    fn a_manual_remedy_is_not_something_fix_can_apply() {
        // The exit-code contract turns on this: a Fail whose only remedy is
        // Manual survives `--fix` and keeps the exit non-zero.
        let mut r = SetupReport::default();
        r.push(c(Status::Fail).with_remedy(Remedy::Manual("brew reinstall".into())));
        assert_eq!(r.fixable().len(), 1);
        assert!(!r.has_automatic_fixes());
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn render_prefixes_every_line_and_shows_remedies_only_where_they_apply() {
        let mut r = SetupReport::default();
        r.push(Check::new("a", "layout", Status::Ok, "fine").with_remedy(Remedy::RunBootstrap));
        r.push(Check::new("b", "relay", Status::Fail, "absent").with_remedy(Remedy::RunBootstrap));
        let text = r.render();
        for line in text.lines() {
            assert!(line.starts_with("[setup]"), "unprefixed line: {line}");
        }
        // Exactly one remedy line: the Ok check's remedy is not advertised.
        let arrows = text.lines().filter(|l| l.trim_start_matches("[setup]").trim_start().starts_with('→')).count();
        assert_eq!(arrows, 1, "{text}");
        assert!(text.contains("INCOMPLETE"));
    }

    #[test]
    fn remedy_descriptions_are_actionable() {
        assert!(Remedy::WriteRelayPin {
            path: PathBuf::from("/h/.claude.json"),
            command: "qd".into(),
        }
        .describe()
        .contains("mcpServers.relay"));
        assert!(Remedy::CreateDirs(vec![PathBuf::from("/h/.quorum")])
            .describe()
            .contains("/h/.quorum"));
    }
}
