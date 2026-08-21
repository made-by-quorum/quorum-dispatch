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

use super::style::Style;

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

    /// The subset of [`fixable`](Self::fixable) an apply pass can perform on
    /// its own — everything except the `Manual` remedies, which no flag can
    /// apply. The ONE definition of "a change setup would make": both
    /// [`has_automatic_fixes`](Self::has_automatic_fixes) (may we prompt?) and
    /// [`render_pending_changes`](Self::render_pending_changes) (what are we
    /// prompting about?) read it, so the prompt and the list under it can never
    /// disagree about what is pending.
    pub fn automatic_fixes(&self) -> Vec<&Check> {
        self.fixable()
            .into_iter()
            .filter(|c| c.remedy.as_ref().is_some_and(Remedy::is_automatic))
            .collect()
    }

    /// Is there anything for an apply pass to actually DO (as opposed to only
    /// `Manual` remedies, which no flag can apply)?
    pub fn has_automatic_fixes(&self) -> bool {
        !self.automatic_fixes().is_empty()
    }

    /// The changes an apply pass would MAKE, as their own section — nothing
    /// else.
    ///
    /// The `→` lines in [`render`](Self::render) answer "what is wrong with
    /// this check": they sit interleaved with a dozen `ok` rows, and they
    /// include the `Manual` remedies, which setup will never perform itself. A
    /// person deciding whether to say `y` is asking a different question — what
    /// is about to be written to my machine — and this is the only place that
    /// answers exactly that. `None` when nothing would be written, which is
    /// also precisely when nothing prompts.
    pub fn render_pending_changes(&self, style: Style) -> Option<String> {
        let pending = self.automatic_fixes();
        if pending.is_empty() {
            return None;
        }
        let prefix = style.dim("[setup]");
        let mut out = format!("{prefix}\n{prefix} {}\n", style.bold("Changes to apply:"));
        for (i, c) in pending.iter().enumerate() {
            out.push_str(&format!(
                "{prefix}   {} {} {}\n",
                style.dim(&format!("{}.", i + 1)),
                style.bold(&format!("{:<14}", c.name)),
                c.remedy.as_ref().expect("automatic_fixes filtered on it").describe()
            ));
        }
        Some(out)
    }

    /// The `[setup]`-prefixed report, one line per check plus the remedy line
    /// under anything that needs one. Prefixed like `qd bootstrap`'s output so
    /// the two read as one first run.
    ///
    /// The verdict trailer is NOT part of this — see
    /// [`render_verdict`](Self::render_verdict) for why they are separate.
    pub fn render(&self, style: Style) -> String {
        let prefix = style.dim("[setup]");
        let mut out = String::new();
        for c in &self.checks {
            // PAD FIRST, then color: the `{:<14}` width has to count the name's
            // characters, not the escape bytes wrapped around them, or every
            // colored column drifts by the length of its own SGR sequence.
            out.push_str(&format!(
                "{prefix} [{}] {} {}\n",
                style.status(c.status, c.status.glyph()),
                style.bold(&format!("{:<14}", c.name)),
                c.detail
            ));
            if c.status.wants_fix() {
                if let Some(r) = &c.remedy {
                    out.push_str(&format!(
                        "{prefix}                        {} {}\n",
                        style.cyan("→"),
                        r.describe()
                    ));
                }
            }
        }
        out
    }

    /// The one-line verdict, with whatever the human must still do to finish.
    ///
    /// SEPARATE FROM [`render`](Self::render) because it is a statement about
    /// how the run ENDED, and the check rows are printed before anyone knows
    /// that. Telling a person "re-run with `--auto-apply-changes`" directly
    /// above a prompt offering to make those very changes for them was the
    /// defect: that advice is only true of a run that is not going to apply
    /// anything — a non-TTY one, or one where the answer was `n`. So the caller
    /// prints this at the point where that question has been settled.
    ///
    /// The instruction also matches what is actually left to do: a report whose
    /// only remaining failures are `Manual` (a broken Homebrew install, an
    /// unparsable `~/.claude.json`) names no flag, because no flag can apply
    /// them.
    pub fn render_verdict(&self, style: Style) -> String {
        let line = if self.exit_code() == 0 {
            style.bold_green("setup: OK")
        } else if self.has_automatic_fixes() {
            style.bold_red("setup: INCOMPLETE — re-run with `qd setup --auto-apply-changes`, or apply the → lines above")
        } else {
            style.bold_red("setup: INCOMPLETE — apply the → lines above")
        };
        format!("{} {line}\n", style.dim("[setup]"))
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
        let text = r.render(Style::PLAIN);
        for line in text.lines() {
            assert!(line.starts_with("[setup]"), "unprefixed line: {line}");
        }
        // Exactly one remedy line: the Ok check's remedy is not advertised.
        let arrows = text.lines().filter(|l| l.trim_start_matches("[setup]").trim_start().starts_with('→')).count();
        assert_eq!(arrows, 1, "{text}");
        // The verdict is the caller's to print, at the point where it is known
        // whether this run is going to apply anything.
        assert!(!text.contains("INCOMPLETE"), "render() is the check rows: {text}");
    }

    /// The verdict names only what is ACTUALLY left to do. A report with an
    /// applicable remedy points at the flag that applies it; one whose failures
    /// are all `Manual` must not, because that flag would change nothing.
    #[test]
    fn the_verdict_only_names_a_flag_that_would_help() {
        let mut ok = SetupReport::default();
        ok.push(c(Status::Ok));
        assert!(ok.render_verdict(Style::PLAIN).contains("setup: OK"));

        let mut manual = SetupReport::default();
        manual.push(c(Status::Fail).with_remedy(Remedy::Manual("brew reinstall".into())));
        let text = manual.render_verdict(Style::PLAIN);
        assert!(text.contains("INCOMPLETE"), "{text}");
        assert!(
            !text.contains("--auto-apply-changes"),
            "nothing here is a change setup can apply: {text}"
        );

        let mut auto = SetupReport::default();
        auto.push(c(Status::Fail).with_remedy(Remedy::RunBootstrap));
        assert!(auto.render_verdict(Style::PLAIN).contains("--auto-apply-changes"));
    }

    /// The section under the prompt lists CHANGES, not checks: the `Ok` row
    /// (nothing to do), the `Manual` row (setup cannot do it) and the
    /// remedy-less `Fail` are all absent, and what is left is numbered in
    /// report order. This is the list a person says `y` to, so anything in it
    /// that setup will not actually write is a lie.
    #[test]
    fn pending_changes_lists_only_what_an_apply_pass_would_write() {
        let mut r = SetupReport::default();
        r.push(Check::new("a", "layout", Status::Ok, "fine").with_remedy(Remedy::RunBootstrap));
        r.push(Check::new("b", "engine-dir", Status::Fail, "absent").with_remedy(Remedy::RunBootstrap));
        r.push(Check::new("c", "brew", Status::Fail, "broken").with_remedy(Remedy::Manual("brew reinstall".into())));
        r.push(Check::new("d", "mystery", Status::Fail, "no idea"));
        r.push(
            Check::new("e", "PATH", Status::Warn, "off PATH").with_remedy(Remedy::WriteRcBlock {
                rc: PathBuf::from("/h/.zshrc"),
                bin_dir: PathBuf::from("/h/.quorum/bin"),
            }),
        );

        let text = r.render_pending_changes(Style::PLAIN).expect("two automatic remedies are pending");
        for line in text.lines() {
            assert!(line.starts_with("[setup]"), "unprefixed line: {line}");
        }
        assert!(text.contains("1. engine-dir"), "{text}");
        assert!(text.contains("2. PATH"), "{text}");
        assert!(!text.contains("layout"), "an Ok check is not a pending change: {text}");
        assert!(!text.contains("brew"), "a Manual remedy is not something setup writes: {text}");
        assert!(!text.contains("mystery"), "a Fail with no remedy has no change to show: {text}");
    }

    /// No section when there is nothing to write — which is exactly when there
    /// is no prompt either. Both read `automatic_fixes`, so they agree by
    /// construction.
    #[test]
    fn nothing_automatic_means_no_section_and_no_prompt() {
        let mut r = SetupReport::default();
        r.push(Check::new("c", "brew", Status::Fail, "broken").with_remedy(Remedy::Manual("brew reinstall".into())));
        assert_eq!(r.render_pending_changes(Style::PLAIN), None);
        assert!(!r.has_automatic_fixes());
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
