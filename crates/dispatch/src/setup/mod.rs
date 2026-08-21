//! `qd setup` — the first run after `brew install` (punch list **R15** + **C2**).
//!
//! # What this module is for
//!
//! Only `quorum-dispatch` ships. `qrm bootstrap` is what does the real
//! first-run install work today, and `qrm` is not part of the shipped product —
//! so that work has to live here. R15 names the four pieces to carry over:
//!
//! 1. the `~/.quorum` directory structure (+ `bin`, `state`), and qd's own
//!    `~/.quorum/dispatch` layout — which `qd bootstrap` ALREADY owns, so setup
//!    calls into it rather than duplicating it;
//! 2. the `qw`-sibling check (ADR-0020);
//! 3. PATH wiring via a managed rc block;
//! 4. the relay pin in `~/.claude.json` — load-bearing for agent messaging.
//!
//! C2 adds the wizard shape on top: `qrm doctor`'s detect-then-`--fix`
//! posture, plus harness detection for Claude Code / codex / pi / opencode
//! (which is also C4's substrate, built here for the first time).
//!
//! # Shape
//!
//! Library-first, matching how [`crate::bootstrap`] relates to
//! `bin/qd/verbs/bootstrap.rs`: everything here is PURE. The bin layer gathers
//! [`SetupFacts`] through the real seams (fs / `Exec` / TTY), [`assess`] turns
//! them into a [`SetupReport`], and the bin applies whichever [`Remedy`] the
//! report attached. No function in this module reads `$HOME`, `$PATH` or
//! `current_exe()` for itself, which is what makes the whole decision table
//! testable against a temp home — the property `qrm`'s tests had, kept.
//!
//! # What is deliberately NOT ported: the `qc` plugin (C17)
//!
//! `qrm bootstrap` also registers the `charter@quorum` Claude Code plugin
//! (`wire_charter` + `rewrite/installed_plugins.rs`). That is NOT carried over.
//! **C17 — "what ships alongside the binaries" — has not been decided**, and
//! the plugin is built from the `qc-plugin` corpus, which is not part of
//! `quorum-dispatch`: porting the registration would mean shipping a verb that
//! registers a plugin this repository cannot produce, and would pre-empt a
//! ruling that is still open (R11 and R12 are blocked on the same question).
//! So setup DETECTS the registration and reports it as one FYI line naming what
//! it would give the user, and stops there. When C17 is ruled, this is the
//! place the decision lands.

pub mod harness;
pub mod layout;
pub mod rc_block;
pub mod relay_pin;
pub mod style;
pub mod verdict;

use std::path::PathBuf;

use harness::{HarnessFacts, HarnessId, Presence};
use layout::{InstallChannel, QuorumLayout, COLOCATED_INTERNAL, SIBLING_ANCHOR};
use relay_pin::PinState;
use verdict::{Check, Remedy, SetupReport, Status};

/// Everything the bin layer probed, as plain data. One struct rather than a
/// dozen arguments so [`assess`] has a single, inspectable input — and so a
/// test can build a whole machine state in one literal.
#[derive(Debug, Clone)]
pub struct SetupFacts {
    /// `$HOME`, injected (never read here).
    pub home: PathBuf,
    pub layout: QuorumLayout,
    /// Which of [`QuorumLayout::owned_dirs`] do not exist yet.
    pub dirs_missing: Vec<PathBuf>,
    /// Does `~/.quorum/dispatch/state` exist? (`qd bootstrap`'s output.)
    pub engine_dir_present: bool,

    /// `current_exe()`, if resolvable.
    pub exe: Option<PathBuf>,
    pub channel: InstallChannel,
    /// ADR-0020: is `qw` a sibling of the running `qd`?
    pub qw_beside_exe: bool,
    /// Are `qd` / `qw` present in `~/.quorum/bin`?
    pub placed_qd: bool,
    pub placed_qw: bool,
    /// From-source only: is the build newer than what is placed in
    /// `~/.quorum/bin`? (A stale placed binary is C14's problem in general;
    /// for the contributor loop it is one `--fix` away, so setup offers it.)
    pub placed_is_stale: bool,

    /// The directory that must be on `PATH` for the bare `qd` relay pin to
    /// resolve.
    pub path_dir: PathBuf,
    /// Is `path_dir` on `$PATH` right now?
    pub path_dir_on_path: bool,
    /// The shell rc file the managed block would go in, if `$SHELL` classified.
    pub rc_path: Option<PathBuf>,
    /// Its current contents (`None` = absent or unreadable).
    pub rc_contents: Option<String>,

    pub claude_json_path: PathBuf,
    pub pin_state: PinState,
    /// For an ABSOLUTE pinned command: does that file still exist? (An absolute
    /// pin that still resolves is left alone — the rule
    /// `relay_server::register::relay_command_is_stale` already encodes.)
    pub pin_command_exists: bool,

    pub harnesses: Vec<HarnessFacts>,
    /// Is the `qc`/charter plugin registered with Claude Code? `None` = could
    /// not tell. FYI only — see the module doc on C17.
    pub qc_plugin_registered: Option<bool>,
}

/// The PURE decision: facts in, verdict out. Every branch here is covered by a
/// unit test with no filesystem.
pub fn assess(f: &SetupFacts) -> SetupReport {
    let mut r = SetupReport::default();

    r.push(check_install(f));
    r.push(check_layout(f));
    r.push(check_engine_dir(f));
    r.push(check_qw_sibling(f));
    r.push(check_placement(f));
    r.push(check_path(f));
    r.push(check_relay_pin(f));
    for h in &f.harnesses {
        r.push(check_harness(h));
    }
    r.push(check_qc_plugin(f));
    r
}

// ---------------------------------------------------------------------------
// install / layout / engine dir
// ---------------------------------------------------------------------------

/// Context, not a verdict: which `qd` is running and where it came from. Every
/// remedy below is channel-specific, so the report opens by saying which
/// channel it decided on.
fn check_install(f: &SetupFacts) -> Check {
    let where_ = f
        .exe
        .as_ref()
        .map(|e| e.display().to_string())
        .unwrap_or_else(|| "(executable path unresolved)".to_string());
    Check::new(
        "install",
        "install",
        Status::Info,
        format!("{} — {}", f.channel.as_str(), where_),
    )
}

/// R15 item 1. `~/.quorum` + `bin` + `state`.
fn check_layout(f: &SetupFacts) -> Check {
    if f.dirs_missing.is_empty() {
        return Check::new(
            "layout",
            "layout",
            Status::Ok,
            format!("{} (+ bin, state)", f.layout.root.display()),
        );
    }
    Check::new(
        "layout",
        "layout",
        Status::Fail,
        format!(
            "missing: {}",
            f.dirs_missing
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .with_remedy(Remedy::CreateDirs(f.dirs_missing.clone()))
}

/// R15 item 1, second half. `qd bootstrap` owns `~/.quorum/dispatch`; setup
/// checks and delegates, and never reimplements it.
fn check_engine_dir(f: &SetupFacts) -> Check {
    if f.engine_dir_present {
        return Check::new(
            "engine-dir",
            "engine-dir",
            Status::Ok,
            format!("{} (owned by `qd bootstrap`)", f.layout.dispatch_home.display()),
        );
    }
    Check::new(
        "engine-dir",
        "engine-dir",
        Status::Fail,
        format!("{} does not exist yet", f.layout.dispatch_home.display()),
    )
    .with_remedy(Remedy::RunBootstrap)
}

// ---------------------------------------------------------------------------
// R15 item 2 — the qw sibling check (ADR-0020)
// ---------------------------------------------------------------------------

/// ADR-0020. An installed `qd` resolves `qw` as a sibling of its OWN executable
/// and never searches `PATH`, so a directory without `qw` beside `qd` is a `qd`
/// that cannot open a lane at all. That is a total loss of function, so it is a
/// `Fail`, never a `Warn`.
///
/// The REMEDY is channel-specific and, under Homebrew, deliberately manual:
/// brew installs both binaries side by side, so a missing `qw` there means the
/// install itself is broken, and copying a binary into a brew-managed prefix
/// would fork the install — two `qd`s upgrading independently, which is the
/// version skew the sibling rule exists to prevent.
fn check_qw_sibling(f: &SetupFacts) -> Check {
    let dir = f.exe.as_ref().and_then(|e| e.parent());
    let dir_s = dir
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    if f.exe.is_none() {
        return Check::new(
            "qw-sibling",
            "qw-sibling",
            Status::Skip,
            "cannot resolve this process's own path — sibling check not possible",
        );
    }
    if f.qw_beside_exe {
        return Check::new(
            "qw-sibling",
            "qw-sibling",
            Status::Ok,
            format!(
                "`{COLOCATED_INTERNAL}` is beside `{SIBLING_ANCHOR}` in {dir_s} \
                 (internal — resolved as a sibling, never via PATH)"
            ),
        );
    }
    let remedy = match f.channel {
        InstallChannel::Homebrew => Remedy::Manual(
            "run `brew reinstall quorum-dispatch` — the formula installs `qd` and `qw` \
             side by side, so a `qw`-less prefix is an incomplete install"
                .to_string(),
        ),
        InstallChannel::FromSource => Remedy::Manual(format!(
            "run `cargo build -p quorum-qw --bin qw` so `qw` lands in {dir_s} \
             (`cargo build -p quorum-dispatch` does not build a dependency's binaries)"
        )),
        InstallChannel::QuorumBin | InstallChannel::Unknown => Remedy::Manual(format!(
            "reinstall qd from a build that contains `qw`, so it lands in {dir_s}"
        )),
    };
    Check::new(
        "qw-sibling",
        "qw-sibling",
        Status::Fail,
        format!(
            "`{COLOCATED_INTERNAL}` is MISSING from {dir_s} — `{SIBLING_ANCHOR}` resolves it as a \
             sibling of its own path and never searches PATH (ADR-0020), so this qd cannot open \
             a lane at all"
        ),
    )
    .with_remedy(remedy)
}

/// The COPY path, kept for the from-source case only (R15 item 2). A
/// contributor running out of `target/release` still wants a working `qd` on
/// PATH, and `~/.quorum/bin` is where it goes.
fn check_placement(f: &SetupFacts) -> Check {
    if !f.channel.places_binaries() {
        return Check::new(
            "placement",
            "placement",
            Status::Skip,
            format!(
                "{} install — the package manager owns binary placement; {} is not used for qd/qw",
                f.channel.as_str(),
                f.layout.bin.display()
            ),
        );
    }
    let src_dir = match f.exe.as_ref().and_then(|e| e.parent()) {
        Some(d) => d.to_path_buf(),
        None => {
            return Check::new(
                "placement",
                "placement",
                Status::Skip,
                "cannot resolve this process's own path",
            )
        }
    };
    let names: Vec<String> = vec![SIBLING_ANCHOR.to_string(), COLOCATED_INTERNAL.to_string()];
    let remedy = Remedy::PlaceBinaries {
        src_dir: src_dir.clone(),
        dst_dir: f.layout.bin.clone(),
        names,
    };
    if !f.placed_qd || !f.placed_qw {
        // Both go together, always: placing `qd` without `qw` builds exactly
        // the broken state the sibling check above exists to catch.
        return Check::new(
            "placement",
            "placement",
            Status::Fail,
            format!(
                "`qd`+`qw` are not both in {} (from-source install)",
                f.layout.bin.display()
            ),
        )
        .with_remedy(remedy);
    }
    if f.placed_is_stale {
        return Check::new(
            "placement",
            "placement",
            Status::Warn,
            format!(
                "{} is older than the build in {} — re-place to pick up your changes",
                f.layout.bin.display(),
                src_dir.display()
            ),
        )
        .with_remedy(remedy);
    }
    Check::new(
        "placement",
        "placement",
        Status::Ok,
        format!("`qd`+`qw` placed in {}", f.layout.bin.display()),
    )
}

// ---------------------------------------------------------------------------
// R15 item 3 — PATH
// ---------------------------------------------------------------------------

/// PATH is a REQUIREMENT, not a convenience, and the reason is the relay pin:
/// `~/.claude.json` stores the BARE command `qd`, which Claude Code resolves
/// via `PATH` when it spawns the relay MCP server. A `qd` that is not on PATH
/// is a machine where agent messaging silently never starts.
///
/// Under Homebrew this is normally already true (brew's `bin` is on PATH), so
/// setup DETECTS that and says so rather than editing an rc file needlessly —
/// R15 item 3's explicit instruction.
fn check_path(f: &SetupFacts) -> Check {
    let dir = f.path_dir.display().to_string();
    if f.path_dir_on_path {
        let why = match f.channel {
            InstallChannel::Homebrew => " (Homebrew's bin — already on PATH, no rc edit needed)",
            _ => "",
        };
        return Check::new("path", "PATH", Status::Ok, format!("{dir} is on PATH{why}"));
    }
    // The rc file already carries our block for this directory, but `$PATH` in
    // THIS process predates it. That is not a fault and there is nothing left
    // to fix — an rc file only takes effect in a new shell. Failing here would
    // make `qd setup --fix` exit non-zero on its own re-assess pass, every
    // time, on a machine it had just finished wiring correctly.
    if f
        .rc_contents
        .as_deref()
        .is_some_and(|c| rc_block::block_exports(c, &dir))
    {
        let rc = f
            .rc_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "your shell rc file".to_string());
        return Check::new(
            "path",
            "PATH",
            Status::Warn,
            format!("{dir} is wired in {rc} but not yet in this shell — open a new shell, or run `source {rc}`"),
        );
    }
    let detail = format!(
        "{dir} is NOT on PATH — the ~/.claude.json relay pin stores the bare command `qd`, \
         which Claude Code resolves via PATH when it launches the relay"
    );
    match &f.rc_path {
        Some(rc) => Check::new("path", "PATH", Status::Fail, detail).with_remedy(Remedy::WriteRcBlock {
            rc: rc.clone(),
            bin_dir: f.path_dir.clone(),
        }),
        // No classifiable $SHELL: we will not guess which file to edit.
        None => Check::new("path", "PATH", Status::Fail, detail).with_remedy(Remedy::Manual(format!(
            "add to your shell profile:  export PATH=\"{dir}:$PATH\"  \
             ($SHELL is unset or not bash/zsh/fish, so setup will not guess the file)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// R15 item 4 — the relay pin
// ---------------------------------------------------------------------------

/// The load-bearing one. Rules the four states of `mcpServers.relay`, reusing
/// the staleness rule already written down in
/// [`crate::relay_server::register::relay_command_is_stale`]: an ABSOLUTE
/// command that still names an existing file is VALID and left alone; a bare
/// command that is not the current one is a stale rename remnant.
fn check_relay_pin(f: &SetupFacts) -> Check {
    let path = f.claude_json_path.display().to_string();
    let fix = Remedy::WriteRelayPin {
        path: f.claude_json_path.clone(),
        command: relay_pin::RELAY_COMMAND.to_string(),
    };
    match &f.pin_state {
        PinState::Absent => Check::new(
            "relay-pin",
            "relay-pin",
            Status::Fail,
            format!("{path} does not exist — no relay pin, so agent messaging cannot start"),
        )
        .with_remedy(fix),
        PinState::Unparsable(e) => Check::new(
            "relay-pin",
            "relay-pin",
            Status::Fail,
            format!("{path} is not valid JSON ({e}) — refusing to rewrite it"),
        )
        .with_remedy(Remedy::Manual(format!(
            "fix the JSON in {path} by hand, then re-run `qd setup --auto-apply-changes`; setup \
             will not clobber a file it cannot parse"
        ))),
        PinState::NoEntry => Check::new(
            "relay-pin",
            "relay-pin",
            Status::Fail,
            format!("{path} has no mcpServers.relay entry"),
        )
        .with_remedy(fix),
        PinState::Entry { command, args } => {
            let args_ok = args.iter().map(String::as_str).eq(relay_pin::RELAY_ARGS.iter().copied());
            let absolute = command.starts_with('/');
            if absolute && !f.pin_command_exists {
                return Check::new(
                    "relay-pin",
                    "relay-pin",
                    Status::Fail,
                    format!("relay -> `{command}`, which no longer exists"),
                )
                .with_remedy(fix);
            }
            if !absolute && command != relay_pin::RELAY_COMMAND {
                return Check::new(
                    "relay-pin",
                    "relay-pin",
                    Status::Fail,
                    format!(
                        "relay -> `{command}` (a stale rename remnant; the command is now `{}`)",
                        relay_pin::RELAY_COMMAND
                    ),
                )
                .with_remedy(fix);
            }
            if !args_ok {
                return Check::new(
                    "relay-pin",
                    "relay-pin",
                    Status::Warn,
                    format!("relay -> `{command}` with args {args:?}, expected [\"relay:serve\"]"),
                )
                .with_remedy(fix);
            }
            let note = if absolute {
                " (absolute, but it still resolves — left alone)"
            } else {
                ""
            };
            Check::new(
                "relay-pin",
                "relay-pin",
                Status::Ok,
                format!("{path} relay -> `{command} relay:serve`{note}"),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// C2 / C4 — harnesses
// ---------------------------------------------------------------------------

/// One line per harness. A harness the human simply does not have is `Info`,
/// never a failure: nobody needs all four, and a setup that failed because you
/// have not installed opencode would be lying about what is wrong.
fn check_harness(h: &HarnessFacts) -> Check {
    let id = match h.id {
        HarnessId::ClaudeCode => "harness.claude",
        HarnessId::Codex => "harness.codex",
        HarnessId::Pi => "harness.pi",
        HarnessId::Opencode => "harness.opencode",
    };
    // The column label IS the harness id — spelled once, in `HarnessId`.
    let name = h.id.as_str();

    if !h.presence.found() {
        return Check::new(
            id,
            name,
            Status::Info,
            format!("not found — with it, qd gives you {}", h.id.offers()),
        );
    }

    let mut detail = String::new();
    if let Some(p) = h.presence.path() {
        detail.push_str(p);
    } else {
        detail.push_str("on PATH");
    }
    if let Some(v) = &h.version {
        detail.push_str(&format!(" ({v})"));
    }
    if !h.pin_note.is_empty() {
        detail.push_str(&format!(" — {}", h.pin_note));
    }
    if !h.wiring_note.is_empty() {
        detail.push_str(&format!(" — {}", h.wiring_note));
    }

    // A pi that is only reachable OFF PATH is the C5 case: qd's launch path
    // reads `QD_PI_BIN` and otherwise runs a bare `pi`, so an off-PATH install
    // is invisible to it until a session fails to start.
    if let Presence::OffPath { path } = &h.presence {
        return Check::new(id, name, Status::Warn, detail).with_remedy(Remedy::Manual(format!(
            "{} is installed but not on PATH; qd runs a bare `{}` unless told otherwise — \
             add to your shell profile:  {}",
            h.id.label(),
            h.id.program(),
            harness::pi_bin_export(path)
        )));
    }
    // Pin drift: real, but never a reason to fail a setup run.
    if h.pin_ok == Some(false) {
        return Check::new(id, name, Status::Warn, detail);
    }
    if h.wired == Some(false) {
        return Check::new(id, name, Status::Warn, detail);
    }
    Check::new(id, name, Status::Ok, detail)
}

/// C17, unresolved — detect and report, never wire. See the module doc.
fn check_qc_plugin(f: &SetupFacts) -> Check {
    let detail = match f.qc_plugin_registered {
        Some(true) => "the `qc` Claude Code plugin is registered (agent onboarding skills: \
                       `orient`, messaging doctrine)"
            .to_string(),
        Some(false) => "the `qc` Claude Code plugin is NOT registered — agent sessions get the \
                        relay MCP instructions only. Not installed by `qd setup`: it is built \
                        from the qc-plugin corpus, which does not ship with these binaries \
                        (punch list C17 is open)"
            .to_string(),
        None => "could not determine whether the `qc` Claude Code plugin is registered".to_string(),
    };
    Check::new("qc-plugin", "qc-plugin", Status::Info, detail)
}

// ---------------------------------------------------------------------------
// --json
// ---------------------------------------------------------------------------

/// The machine-readable report: the detected state AND the verdicts, so a
/// consumer can act on either. `id` fields are the stable keys; `name`/`detail`
/// are prose and may change.
pub fn to_json(f: &SetupFacts, r: &SetupReport) -> serde_json::Value {
    let checks: Vec<serde_json::Value> = r
        .checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "status": c.status.as_str(),
                "detail": c.detail,
                "remedy": c.remedy.as_ref().map(|x| x.describe()),
                "auto_fixable": c.status.wants_fix()
                    && c.remedy.as_ref().is_some_and(|x| x.is_automatic()),
            })
        })
        .collect();
    let harnesses: Vec<serde_json::Value> = f
        .harnesses
        .iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id.as_str(),
                "found": h.presence.found(),
                "on_path": matches!(h.presence, Presence::OnPath { .. }),
                "path": h.presence.path(),
                "version": h.version,
                "pin_ok": h.pin_ok,
                "pin_note": if h.pin_note.is_empty() { None } else { Some(&h.pin_note) },
                "wired": h.wired,
            })
        })
        .collect();
    serde_json::json!({
        "ok": r.exit_code() == 0,
        "exit_code": r.exit_code(),
        "home": f.home.display().to_string(),
        "install": {
            "channel": f.channel.as_str(),
            "exe": f.exe.as_ref().map(|e| e.display().to_string()),
            "qw_beside_exe": f.qw_beside_exe,
        },
        "layout": {
            "root": f.layout.root.display().to_string(),
            "bin": f.layout.bin.display().to_string(),
            "state": f.layout.state.display().to_string(),
            "dispatch_home": f.layout.dispatch_home.display().to_string(),
            "missing": f.dirs_missing.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        },
        "path": {
            "dir": f.path_dir.display().to_string(),
            "on_path": f.path_dir_on_path,
            "rc_file": f.rc_path.as_ref().map(|p| p.display().to_string()),
        },
        "relay_pin": {
            "file": f.claude_json_path.display().to_string(),
            "state": f.pin_state.as_str(),
        },
        "harnesses": harnesses,
        "qc_plugin_registered": f.qc_plugin_registered,
        "checks": checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use style::Style;

    /// A machine where absolutely everything is already right. Individual tests
    /// break ONE thing, so each assertion is about one decision.
    fn healthy() -> SetupFacts {
        let home = PathBuf::from("/home/u");
        let layout = QuorumLayout::resolve(&home, None);
        SetupFacts {
            home,
            dirs_missing: vec![],
            engine_dir_present: true,
            exe: Some(PathBuf::from("/opt/homebrew/bin/qd")),
            channel: InstallChannel::Homebrew,
            qw_beside_exe: true,
            placed_qd: false,
            placed_qw: false,
            placed_is_stale: false,
            path_dir: PathBuf::from("/opt/homebrew/bin"),
            path_dir_on_path: true,
            rc_path: Some(PathBuf::from("/home/u/.zshrc")),
            rc_contents: Some(String::new()),
            claude_json_path: PathBuf::from("/home/u/.claude.json"),
            pin_state: PinState::Entry {
                command: "qd".into(),
                args: vec!["relay:serve".into()],
            },
            pin_command_exists: true,
            harnesses: vec![HarnessFacts {
                id: HarnessId::ClaudeCode,
                presence: Presence::OnPath {
                    path: Some("/usr/local/bin/claude".into()),
                },
                version: Some("2.1.0".into()),
                pin_ok: None,
                pin_note: String::new(),
                wired: Some(true),
                wiring_note: "relay pinned".into(),
            }],
            qc_plugin_registered: Some(false),
            layout,
        }
    }

    fn status_of(r: &SetupReport, id: &str) -> Status {
        r.checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no check {id}")).status
    }

    fn check_of<'a>(r: &'a SetupReport, id: &str) -> &'a Check {
        r.checks.iter().find(|c| c.id == id).unwrap()
    }

    #[test]
    fn a_fully_wired_machine_is_green_and_exits_zero() {
        let r = assess(&healthy());
        assert_eq!(r.exit_code(), 0, "{}", r.render(Style::PLAIN));
        assert_eq!(status_of(&r, "layout"), Status::Ok);
        assert_eq!(status_of(&r, "engine-dir"), Status::Ok);
        assert_eq!(status_of(&r, "qw-sibling"), Status::Ok);
        assert_eq!(status_of(&r, "path"), Status::Ok);
        assert_eq!(status_of(&r, "relay-pin"), Status::Ok);
        // Homebrew never copies binaries.
        assert_eq!(status_of(&r, "placement"), Status::Skip);
    }

    /// The invariant the top-level help's cheap probe rests on
    /// (`bin/qd/verbs/setup.rs::install_is_incomplete`): a harness is never a
    /// FAIL, so the exit code — and therefore the "is this machine set up"
    /// answer — does not depend on the harness probes at all. That is what lets
    /// `qd --help` answer the question without shelling out eight times.
    ///
    /// MUTATION EVIDENCE: making any `check_harness` arm return `Status::Fail`
    /// reds this, and correctly so — the probe would then have to run.
    #[test]
    fn harness_checks_never_gate_the_exit_code() {
        // Every presence/version/wiring shape a probe can produce, including the
        // ones that carry a WARN.
        let shapes = [
            HarnessFacts {
                id: HarnessId::ClaudeCode,
                presence: Presence::Missing,
                version: None,
                pin_ok: None,
                pin_note: String::new(),
                wired: None,
                wiring_note: String::new(),
            },
            HarnessFacts {
                id: HarnessId::Codex,
                presence: Presence::OnPath { path: None },
                version: None,
                pin_ok: Some(false),
                pin_note: "breaking drift".into(),
                wired: Some(false),
                wiring_note: "not wired".into(),
            },
            HarnessFacts {
                id: HarnessId::Pi,
                presence: Presence::OffPath {
                    path: "/usr/local/bin/pi".into(),
                },
                version: Some("0.1.0".into()),
                pin_ok: Some(true),
                pin_note: String::new(),
                wired: Some(true),
                wiring_note: "wired".into(),
            },
        ];
        for h in &shapes {
            let mut f = healthy();
            f.harnesses = vec![h.clone()];
            let r = assess(&f);
            assert_eq!(r.exit_code(), 0, "{:?} gated the exit code:\n{}", h.id, r.render(Style::PLAIN));
        }
        // ...and a machine with NO harness facts at all — what the help's probe
        // assesses — reaches the same verdict as the fully probed one.
        let mut none = healthy();
        none.harnesses = vec![];
        assert_eq!(assess(&none).exit_code(), assess(&healthy()).exit_code());
        let mut broken = healthy();
        broken.engine_dir_present = false;
        let mut broken_unprobed = broken.clone();
        broken_unprobed.harnesses = vec![];
        assert_eq!(assess(&broken_unprobed).exit_code(), assess(&broken).exit_code());
        assert_eq!(assess(&broken_unprobed).exit_code(), 1);
    }

    #[test]
    fn every_check_id_is_unique_so_json_consumers_can_key_off_it() {
        let r = assess(&healthy());
        let mut ids: Vec<_> = r.checks.iter().map(|c| c.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate check id");
    }

    // --- layout / engine dir ------------------------------------------------

    #[test]
    fn missing_dirs_fail_and_offer_to_create_exactly_those() {
        let mut f = healthy();
        f.dirs_missing = vec![f.layout.root.clone(), f.layout.bin.clone()];
        let r = assess(&f);
        let c = check_of(&r, "layout");
        assert_eq!(c.status, Status::Fail);
        assert_eq!(
            c.remedy,
            Some(Remedy::CreateDirs(vec![
                PathBuf::from("/home/u/.quorum"),
                PathBuf::from("/home/u/.quorum/bin")
            ]))
        );
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn the_engine_dir_is_delegated_to_qd_bootstrap_never_reimplemented() {
        let mut f = healthy();
        f.engine_dir_present = false;
        let r = assess(&f);
        let c = check_of(&r, "engine-dir");
        assert_eq!(c.status, Status::Fail);
        assert_eq!(c.remedy, Some(Remedy::RunBootstrap));
    }

    // --- ADR-0020 -----------------------------------------------------------

    #[test]
    fn a_missing_qw_is_a_hard_fail_not_a_warning() {
        // ADR-0020: it is a total loss of function, so understating it as a
        // Warn would let a broken install exit 0.
        let mut f = healthy();
        f.qw_beside_exe = false;
        let r = assess(&f);
        let c = check_of(&r, "qw-sibling");
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("never searches PATH"), "{}", c.detail);
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn the_missing_qw_remedy_is_channel_specific_and_never_copies_under_brew() {
        let mut f = healthy();
        f.qw_beside_exe = false;

        f.channel = InstallChannel::Homebrew;
        let c = check_of(&assess(&f), "qw-sibling").clone();
        match c.remedy.unwrap() {
            Remedy::Manual(m) => assert!(m.contains("brew reinstall"), "{m}"),
            other => panic!("brew must be check-and-explain, got {other:?}"),
        }

        f.channel = InstallChannel::FromSource;
        f.exe = Some(PathBuf::from("/repo/target/release/qd"));
        let c = check_of(&assess(&f), "qw-sibling").clone();
        match c.remedy.unwrap() {
            Remedy::Manual(m) => assert!(m.contains("cargo build -p quorum-qw"), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unresolvable_exe_skips_the_sibling_check_rather_than_guessing() {
        let mut f = healthy();
        f.exe = None;
        let r = assess(&f);
        assert_eq!(status_of(&r, "qw-sibling"), Status::Skip);
        assert_eq!(r.exit_code(), 0);
    }

    // --- placement (from-source only) ---------------------------------------

    fn from_source() -> SetupFacts {
        let mut f = healthy();
        f.channel = InstallChannel::FromSource;
        f.exe = Some(PathBuf::from("/repo/target/release/qd"));
        f.path_dir = f.layout.bin.clone();
        f
    }

    #[test]
    fn from_source_places_both_binaries_together() {
        let mut f = from_source();
        f.placed_qd = true;
        f.placed_qw = false; // the exact half-installed state ADR-0020 warns about
        let c = check_of(&assess(&f), "placement").clone();
        assert_eq!(c.status, Status::Fail);
        match c.remedy.unwrap() {
            Remedy::PlaceBinaries { src_dir, dst_dir, names } => {
                assert_eq!(src_dir, Path::new("/repo/target/release"));
                assert_eq!(dst_dir, Path::new("/home/u/.quorum/bin"));
                assert_eq!(names, vec!["qd".to_string(), "qw".to_string()]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_stale_from_source_placement_warns_and_is_fixable_without_failing() {
        let mut f = from_source();
        f.placed_qd = true;
        f.placed_qw = true;
        f.placed_is_stale = true;
        let r = assess(&f);
        let c = check_of(&r, "placement");
        assert_eq!(c.status, Status::Warn);
        assert!(c.remedy.as_ref().unwrap().is_automatic());
        assert_eq!(r.exit_code(), 0, "staleness must not fail the run");
    }

    #[test]
    fn a_current_from_source_placement_is_ok() {
        let mut f = from_source();
        f.placed_qd = true;
        f.placed_qw = true;
        assert_eq!(status_of(&assess(&f), "placement"), Status::Ok);
    }

    // --- PATH ---------------------------------------------------------------

    #[test]
    fn a_homebrew_bin_already_on_path_says_so_instead_of_editing_the_rc_file() {
        let r = assess(&healthy());
        let c = check_of(&r, "path");
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("no rc edit needed"), "{}", c.detail);
        assert!(c.remedy.is_none(), "must not offer to edit a file it need not touch");
    }

    #[test]
    fn a_dir_off_path_fails_and_names_the_relay_as_the_reason() {
        let mut f = healthy();
        f.path_dir_on_path = false;
        let r = assess(&f);
        let c = check_of(&r, "path");
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("relay"), "{}", c.detail);
        assert_eq!(
            c.remedy,
            Some(Remedy::WriteRcBlock {
                rc: PathBuf::from("/home/u/.zshrc"),
                bin_dir: PathBuf::from("/opt/homebrew/bin"),
            })
        );
    }

    #[test]
    fn an_already_written_block_that_this_shell_has_not_sourced_is_a_warn_not_a_fail() {
        // Otherwise `qd setup --fix` would exit 1 on its own re-assess pass,
        // every time, on a machine it had just wired correctly: an rc file
        // only takes effect in a NEW shell.
        let mut f = healthy();
        f.path_dir_on_path = false;
        f.rc_contents = Some(rc_block::upsert_block("", "/opt/homebrew/bin"));
        let r = assess(&f);
        let c = check_of(&r, "path");
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("open a new shell"), "{}", c.detail);
        assert!(c.remedy.is_none(), "nothing left to write");
        assert_eq!(r.exit_code(), 0);

        // A block pointing somewhere ELSE does not count as wired.
        f.rc_contents = Some(rc_block::upsert_block("", "/some/other/bin"));
        assert_eq!(status_of(&assess(&f), "path"), Status::Fail);
    }

    #[test]
    fn without_a_classifiable_shell_setup_refuses_to_guess_the_rc_file() {
        let mut f = healthy();
        f.path_dir_on_path = false;
        f.rc_path = None;
        let c = check_of(&assess(&f), "path").clone();
        match c.remedy.unwrap() {
            Remedy::Manual(m) => assert!(m.contains("export PATH="), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- relay pin ----------------------------------------------------------

    #[test]
    fn all_three_unpinned_states_fail_with_an_automatic_fix() {
        for state in [PinState::Absent, PinState::NoEntry] {
            let mut f = healthy();
            f.pin_state = state.clone();
            let c = check_of(&assess(&f), "relay-pin").clone();
            assert_eq!(c.status, Status::Fail, "{state:?}");
            assert!(c.remedy.as_ref().unwrap().is_automatic(), "{state:?}");
        }
    }

    #[test]
    fn an_unparsable_claude_json_is_never_rewritten() {
        let mut f = healthy();
        f.pin_state = PinState::Unparsable("expected value at line 1".into());
        let c = check_of(&assess(&f), "relay-pin").clone();
        assert_eq!(c.status, Status::Fail);
        assert!(
            !c.remedy.as_ref().unwrap().is_automatic(),
            "must not clobber a file it cannot parse"
        );
    }

    #[test]
    fn a_stale_bare_command_is_repointed() {
        let mut f = healthy();
        f.pin_state = PinState::Entry {
            command: "dispatch".into(), // the pre-rename remnant
            args: vec!["relay:serve".into()],
        };
        let c = check_of(&assess(&f), "relay-pin").clone();
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("stale rename"), "{}", c.detail);
    }

    #[test]
    fn an_absolute_pin_that_still_resolves_is_left_alone() {
        // register::relay_command_is_stale's rule, carried over: a legacy
        // absolute entry that still works is valid.
        let mut f = healthy();
        f.pin_state = PinState::Entry {
            command: "/old/bin/qd".into(),
            args: vec!["relay:serve".into()],
        };
        f.pin_command_exists = true;
        let c = check_of(&assess(&f), "relay-pin").clone();
        assert_eq!(c.status, Status::Ok);
        assert!(c.remedy.is_none());

        f.pin_command_exists = false;
        let c = check_of(&assess(&f), "relay-pin").clone();
        assert_eq!(c.status, Status::Fail);
        assert!(c.remedy.unwrap().is_automatic());
    }

    #[test]
    fn wrong_args_warn_rather_than_fail() {
        let mut f = healthy();
        f.pin_state = PinState::Entry {
            command: "qd".into(),
            args: vec![],
        };
        let r = assess(&f);
        assert_eq!(status_of(&r, "relay-pin"), Status::Warn);
        assert_eq!(r.exit_code(), 0);
    }

    // --- harnesses ----------------------------------------------------------

    #[test]
    fn a_missing_harness_is_an_fyi_that_names_what_it_would_give_you() {
        let mut f = healthy();
        f.harnesses = vec![HarnessFacts::new(HarnessId::Opencode, Presence::Missing)];
        let r = assess(&f);
        let c = check_of(&r, "harness.opencode");
        assert_eq!(c.status, Status::Info, "not having a harness is not a failure");
        assert!(c.detail.contains("opencode sessions over ACP"), "{}", c.detail);
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn an_off_path_pi_warns_with_the_exact_export_c5_asks_for() {
        let mut f = healthy();
        f.harnesses = vec![HarnessFacts::new(
            HarnessId::Pi,
            Presence::OffPath {
                path: "/home/u/.npm-pi-global/bin/pi".into(),
            },
        )];
        let r = assess(&f);
        let c = check_of(&r, "harness.pi");
        assert_eq!(c.status, Status::Warn);
        match c.remedy.clone().unwrap() {
            Remedy::Manual(m) => {
                assert!(m.contains(r#"export QD_PI_BIN="/home/u/.npm-pi-global/bin/pi""#), "{m}")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(r.exit_code(), 0, "an off-PATH pi is reportable, not fatal");
    }

    #[test]
    fn version_drift_warns_and_is_rendered_into_the_detail() {
        let mut f = healthy();
        let mut h = HarnessFacts::new(
            HarnessId::Codex,
            Presence::OnPath {
                path: Some("/usr/local/bin/codex".into()),
            },
        );
        h.version = Some("0.200.0".into());
        h.pin_ok = Some(false);
        h.pin_note = "BREAKING drift: qd is pinned to 0.146.1".into();
        f.harnesses = vec![h];
        let r = assess(&f);
        let c = check_of(&r, "harness.codex");
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("0.200.0"), "{}", c.detail);
        assert!(c.detail.contains("BREAKING"), "{}", c.detail);
        assert_eq!(r.exit_code(), 0);
    }

    // --- C17 ----------------------------------------------------------------

    #[test]
    fn the_qc_plugin_is_reported_never_wired() {
        let mut f = healthy();
        f.qc_plugin_registered = Some(false);
        let r = assess(&f);
        let c = check_of(&r, "qc-plugin");
        assert_eq!(c.status, Status::Info, "C17 is open — this must not gate anything");
        assert!(c.remedy.is_none(), "setup must not offer to install it");
        assert!(c.detail.contains("C17"), "{}", c.detail);

        f.qc_plugin_registered = Some(true);
        assert_eq!(status_of(&assess(&f), "qc-plugin"), Status::Info);
        f.qc_plugin_registered = None;
        assert_eq!(status_of(&assess(&f), "qc-plugin"), Status::Info);
    }

    // --- json ---------------------------------------------------------------

    #[test]
    fn json_carries_the_state_and_the_verdicts() {
        let f = healthy();
        let r = assess(&f);
        let v = to_json(&f, &r);
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["exit_code"], serde_json::json!(0));
        assert_eq!(v["install"]["channel"], serde_json::json!("homebrew"));
        assert_eq!(v["relay_pin"]["state"], serde_json::json!("entry"));
        assert_eq!(v["harnesses"][0]["id"], serde_json::json!("claude"));
        assert_eq!(v["harnesses"][0]["found"], serde_json::json!(true));
        let checks = v["checks"].as_array().unwrap();
        assert!(checks.iter().any(|c| c["id"] == serde_json::json!("relay-pin")));
        // Serializes cleanly (no NaN / non-UTF8 path panics).
        assert!(serde_json::to_string(&v).is_ok());
    }

    #[test]
    fn json_marks_which_checks_fix_could_actually_apply() {
        let mut f = healthy();
        f.pin_state = PinState::Absent; // automatic
        f.qw_beside_exe = false; // manual
        let r = assess(&f);
        let v = to_json(&f, &r);
        let checks = v["checks"].as_array().unwrap();
        let find = |id: &str| {
            checks
                .iter()
                .find(|c| c["id"] == serde_json::json!(id))
                .unwrap()
                .clone()
        };
        assert_eq!(find("relay-pin")["auto_fixable"], serde_json::json!(true));
        assert_eq!(find("qw-sibling")["auto_fixable"], serde_json::json!(false));
        assert_eq!(v["exit_code"], serde_json::json!(1));
    }
}
