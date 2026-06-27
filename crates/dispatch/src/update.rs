//! `qd update` — self-update (A5 spec §4.3; named divergence §9 item 3).
//!
//! FRESH DESIGN, not a port: TS `qd update` ran `bun install -g <repo>`
//! (0d0fa9e:src/commands/update.ts). The Rust engine ships via cargo or
//! Homebrew, so the update mechanism is rebuilt around those two channels:
//!
//!   - exe under a Homebrew prefix (`*/Cellar/*` ancestry) → `brew upgrade qd`
//!     (argv-level only until A7 lands the formula).
//!   - exe under `~/.cargo/bin`                            → `cargo install
//!     --git <repo> --locked qd` (repo = the workspace Cargo.toml `repository`).
//!   - neither                                            → guidance + exit 1.
//!
//! Library-first (spec §2): [`decide_update_action`] is PURE over the resolved
//! exe path + env; the runtime ([`run_update`]) is a thin shell over an injected
//! exec seam, so unit tests assert the constructed argv + channel detection on
//! EVERY branch WITHOUT running a real `cargo`/`brew` (the TS test
//! "qd update (injected exec — never runs real bun)" equivalent). Exit codes are
//! 0/1 ONLY (ADR 0008): a clean update inherits the child's exit; a channel we
//! cannot determine is exit 1.

use std::path::Path;

/// The workspace repository (Cargo.toml `repository` field). Used to build the
/// `cargo install --git <repo>` argv. Kept as a const here AND read from the
/// real manifest by the bin layer so the two never drift.
pub const REPO_URL: &str = "https://github.com/private-org/qd-rust";

/// The Homebrew formula name (A7 lands the real formula; until then the
/// `brew upgrade` path is argv-level only).
pub const BREW_FORMULA: &str = "dispatch";

/// The cargo crate name installed via `cargo install`.
pub const CARGO_CRATE: &str = "dispatch";

/// The resolved update channel + the argv to run, OR a hard error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    /// `brew upgrade qd`.
    Brew { argv: Vec<String> },
    /// `cargo install --git <repo> --locked qd`.
    Cargo { argv: Vec<String> },
    /// Could not determine the channel → guidance + exit 1.
    Unknown { message: String },
}

/// PURE channel decider over the resolved exe path + the repo url.
///
/// Detection:
///   - Homebrew: the exe path contains a `/Cellar/` segment (the canonical
///     Homebrew install layout `<prefix>/Cellar/<formula>/<ver>/bin/dispatch`), OR it
///     sits under the `brew --prefix` ancestry passed in `brew_prefix`. The
///     `brew --prefix` probe is the bin layer's job (it shells out); the pure
///     decider takes the already-resolved prefix string so it stays testable.
///   - cargo: the exe path sits under `<cargo_bin>` (real: `~/.cargo/bin`,
///     resolved by the bin layer from HOME/CARGO_HOME so this stays pure).
///
/// Homebrew is checked first (a brew-installed binary can technically live under
/// a path that also matches other heuristics; the Cellar layout is unambiguous).
pub fn decide_update_action(
    exe_path: &Path,
    brew_prefix: Option<&str>,
    cargo_bin: Option<&Path>,
    repo_url: &str,
) -> UpdateAction {
    let exe_str = exe_path.to_string_lossy();

    // Homebrew: `*/Cellar/*` segment OR under the brew --prefix ancestry.
    let cellar = path_has_segment(exe_path, "Cellar");
    let under_brew_prefix = brew_prefix
        .filter(|p| !p.is_empty())
        .is_some_and(|p| path_starts_with_str(&exe_str, p));
    if cellar || under_brew_prefix {
        return UpdateAction::Brew {
            argv: vec![
                "brew".to_string(),
                "upgrade".to_string(),
                BREW_FORMULA.to_string(),
            ],
        };
    }

    // cargo: under ~/.cargo/bin.
    if let Some(cargo_bin) = cargo_bin {
        if exe_path.starts_with(cargo_bin) {
            return UpdateAction::Cargo {
                argv: vec![
                    "cargo".to_string(),
                    "install".to_string(),
                    "--git".to_string(),
                    repo_url.to_string(),
                    "--locked".to_string(),
                    CARGO_CRATE.to_string(),
                ],
            };
        }
    }

    UpdateAction::Unknown {
        message: format!(
            "dispatch update: cannot determine install channel (expected Homebrew or cargo); \
             reinstall manually from {repo_url}."
        ),
    }
}

/// Does any path component equal `seg`? (Used for the `/Cellar/` detection —
/// component-wise so a directory literally named `Cellar` matches but a
/// substring like `MyCellarThing` does not.)
fn path_has_segment(path: &Path, seg: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy() == seg)
}

/// String prefix that respects a `/` boundary: `prefix` must be a path-segment
/// prefix of `s` (so `/opt/homebrew` matches `/opt/homebrew/bin/dispatch` but not
/// `/opt/homebrew-x/...`).
fn path_starts_with_str(s: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if let Some(rest) = s.strip_prefix(prefix) {
        rest.is_empty() || rest.starts_with('/')
    } else {
        false
    }
}

/// Injected exec seam for the update runtime: run argv with inherited stdio,
/// return the child exit code. Separate from [`crate::exec::Exec`] so the unit
/// tests can assert the exact argv with a tiny closure-backed double.
pub trait UpdateExec {
    fn run_inherit(&self, argv: &[String]) -> i32;
}

/// The update runtime result: the resolved action + the exit code to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub action: UpdateAction,
    /// The exit code the verb returns (child's exit on a real channel; 1 on
    /// Unknown). 0/1 only on the Unknown path; a child may return anything but
    /// the bin layer clamps non-{0,1} to 1 (ADR 0008).
    pub exit_code: i32,
    /// The `[update]`-prefixed report lines (engine-truthful, 3-line shape).
    pub report: Vec<String>,
}

/// Run the update: decide the channel, print the `[update]` 3-line report, and
/// (on a real channel) exec the argv via the seam. Returns the outcome. On
/// Unknown the message goes to the report and the exit code is 1; NO exec runs.
pub fn run_update(action: UpdateAction, exec: &dyn UpdateExec) -> UpdateOutcome {
    match &action {
        UpdateAction::Unknown { message } => UpdateOutcome {
            report: vec![message.clone()],
            exit_code: 1,
            action,
        },
        UpdateAction::Brew { argv } | UpdateAction::Cargo { argv } => {
            let channel = match &action {
                UpdateAction::Brew { .. } => "Homebrew",
                _ => "cargo",
            };
            let cmd = argv.join(" ");
            // 3-line shape, mirroring the TS `[update]` reporter (update.ts:99-114)
            // with engine-truthful text: detected → running → (child exit decides
            // the final line, printed by the bin layer after exec).
            let report = vec![
                format!("[update] channel: {channel}"),
                format!("[update] running: {cmd}"),
            ];
            let exit_code = exec.run_inherit(argv);
            UpdateOutcome {
                action,
                exit_code,
                report,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    struct ExecSpy {
        ran: RefCell<Option<Vec<String>>>,
        exit: i32,
    }
    impl UpdateExec for ExecSpy {
        fn run_inherit(&self, argv: &[String]) -> i32 {
            *self.ran.borrow_mut() = Some(argv.to_vec());
            self.exit
        }
    }
    fn spy(exit: i32) -> ExecSpy {
        ExecSpy {
            ran: RefCell::new(None),
            exit,
        }
    }

    // --- decide_update_action: EVERY branch -------------------------------

    #[test]
    fn brew_detected_via_cellar_segment() {
        let exe = PathBuf::from("/opt/homebrew/Cellar/dispatch/0.1.0/bin/dispatch");
        let a = decide_update_action(&exe, None, None, REPO_URL);
        assert_eq!(
            a,
            UpdateAction::Brew {
                argv: vec!["brew".into(), "upgrade".into(), "dispatch".into()]
            }
        );
    }

    #[test]
    fn brew_detected_via_prefix_ancestry() {
        let exe = PathBuf::from("/opt/homebrew/bin/dispatch");
        let a = decide_update_action(&exe, Some("/opt/homebrew"), None, REPO_URL);
        assert!(matches!(a, UpdateAction::Brew { .. }));
    }

    #[test]
    fn brew_prefix_respects_segment_boundary() {
        // /opt/homebrew-x must NOT match prefix /opt/homebrew.
        let exe = PathBuf::from("/opt/homebrew-x/bin/dispatch");
        let a = decide_update_action(&exe, Some("/opt/homebrew"), None, REPO_URL);
        assert!(matches!(a, UpdateAction::Unknown { .. }));
    }

    #[test]
    fn cargo_detected_under_cargo_bin() {
        let exe = PathBuf::from("/home/u/.cargo/bin/dispatch");
        let cargo_bin = PathBuf::from("/home/u/.cargo/bin");
        let a = decide_update_action(&exe, None, Some(&cargo_bin), REPO_URL);
        assert_eq!(
            a,
            UpdateAction::Cargo {
                argv: vec![
                    "cargo".into(),
                    "install".into(),
                    "--git".into(),
                    REPO_URL.into(),
                    "--locked".into(),
                    "dispatch".into(),
                ]
            }
        );
    }

    #[test]
    fn unknown_channel_when_neither() {
        let exe = PathBuf::from("/usr/local/bin/dispatch");
        let cargo_bin = PathBuf::from("/home/u/.cargo/bin");
        let a = decide_update_action(&exe, Some("/opt/homebrew"), Some(&cargo_bin), REPO_URL);
        match a {
            UpdateAction::Unknown { message } => {
                assert!(message.contains("cannot determine install channel"));
                assert!(message.contains(REPO_URL));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn brew_wins_over_cargo_when_both_match() {
        // A Cellar exe whose path also happens to live under a cargo_bin prefix:
        // Homebrew is checked first.
        let exe = PathBuf::from("/opt/homebrew/Cellar/dispatch/0.1.0/bin/dispatch");
        let cargo_bin = PathBuf::from("/opt/homebrew/Cellar"); // contrived overlap
        let a = decide_update_action(&exe, None, Some(&cargo_bin), REPO_URL);
        assert!(matches!(a, UpdateAction::Brew { .. }));
    }

    // --- run_update: injected exec, never runs real cargo/brew ------------

    #[test]
    fn run_update_brew_execs_argv_inherits_exit() {
        let exe = PathBuf::from("/opt/homebrew/Cellar/dispatch/0.1.0/bin/dispatch");
        let action = decide_update_action(&exe, None, None, REPO_URL);
        let s = spy(0);
        let out = run_update(action, &s);
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            s.ran.borrow().as_deref(),
            Some(["brew", "upgrade", "dispatch"].map(String::from).as_slice())
        );
        assert!(out.report.iter().any(|l| l.contains("Homebrew")));
    }

    #[test]
    fn run_update_cargo_execs_argv_inherits_nonzero_exit() {
        let exe = PathBuf::from("/home/u/.cargo/bin/dispatch");
        let cargo_bin = PathBuf::from("/home/u/.cargo/bin");
        let action = decide_update_action(&exe, None, Some(&cargo_bin), REPO_URL);
        let s = spy(3);
        let out = run_update(action, &s);
        assert_eq!(out.exit_code, 3);
        let ran = s.ran.borrow();
        let ran = ran.as_deref().unwrap();
        assert_eq!(ran[0], "cargo");
        assert!(ran.contains(&REPO_URL.to_string()));
        assert!(ran.contains(&"--locked".to_string()));
    }

    #[test]
    fn run_update_unknown_never_execs_exit_1() {
        let action = UpdateAction::Unknown {
            message:
                "dispatch update: cannot determine install channel (expected Homebrew or cargo); \
                      reinstall manually from x."
                    .to_string(),
        };
        let s = spy(0);
        let out = run_update(action, &s);
        assert_eq!(out.exit_code, 1);
        assert!(s.ran.borrow().is_none(), "Unknown must NEVER exec");
    }
}
