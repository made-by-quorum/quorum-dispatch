//! The `~/.quorum` install layout, and WHICH INSTALL this `qd` is (R15 items
//! 1 and 2).
//!
//! Ported from `qrm/src/paths.rs` + `qrm/src/binaries.rs`. This is a PORT, not
//! a reuse: `qrm` is not part of the shipped product (punch list "Shipping
//! shape"), so `quorum-dispatch` cannot depend on that crate and has to carry
//! the layout itself. What is deliberately NOT carried over:
//!
//! - **`frame_home` / `FRAME_ROOT`.** `qf` does not ship either, so a layout
//!   field for its ledger root would be describing a directory nothing writes.
//! - **The four-binary roster.** `qrm/src/binaries.rs` tables `qd`, `qf`,
//!   `qrm`, `qbt`, `qw`. Only `qd` and `qw` ship, and the only distinction that
//!   survives is the one ADR-0020 is about — see [`SIBLING_ANCHOR`].
//!
//! Everything here takes `home` as an argument rather than reading `$HOME`, so
//! the whole module is testable against a temp home and no test can touch the
//! real `~` (the property `qrm`'s own tests had, kept).

use std::path::{Path, PathBuf};

/// The binary an internal colocated binary must sit BESIDE. `qd` is the process
/// that resolves `qw` as a sibling of its own executable.
pub const SIBLING_ANCHOR: &str = "qd";

/// The internal colocated binary (ADR-0020,
/// `dispatch/doc/adr/0020-qw-is-a-sibling-binary-not-a-fourth-package.md`).
///
/// `quorum_qw::wire::client::resolve_qw` finds `qw` as a sibling of `qd`'s own
/// `current_exe()` and NEVER searches `PATH`, because a `qw` on `PATH` could
/// come from a different install than the running `qd` — the exact version skew
/// the wire handshake exists to catch. So the check setup runs is "is `qw`
/// beside the running `qd`", never "does `qw` resolve": the latter asserts the
/// wrong property and passes in states that are broken.
pub const COLOCATED_INTERNAL: &str = "qw";

/// Resolved layout of the qd install (shrunk port of `qrm`'s `Layout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumLayout {
    /// `~/.quorum`
    pub root: PathBuf,
    /// `~/.quorum/bin` — where a FROM-SOURCE install places `qd` + `qw`, and
    /// the directory the managed PATH block exports. A Homebrew install does
    /// not use it (brew owns its own `bin`), but it is created either way: it
    /// costs one empty directory and it means the two install shapes have the
    /// same layout on disk.
    pub bin: PathBuf,
    /// `~/.quorum/state` — the suite-level scratch dir `qrm bootstrap` created.
    pub state: PathBuf,
    /// `~/.quorum/dispatch` (or `$QD_HOME`) — the ENGINE data dir. Owned by
    /// `qd bootstrap`; setup only reports on it and delegates its creation.
    pub dispatch_home: PathBuf,
}

impl QuorumLayout {
    /// Compute the layout from an explicit home + the `QD_HOME` override.
    /// `qd_home` is passed in (already read through the `Env` seam) so this
    /// stays pure.
    pub fn resolve(home: &Path, qd_home: Option<&str>) -> Self {
        let root = home.join(".quorum");
        let dispatch_home = match qd_home.filter(|s| !s.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => root.join("dispatch"),
        };
        QuorumLayout {
            bin: root.join("bin"),
            state: root.join("state"),
            root,
            dispatch_home,
        }
    }

    /// The directories setup itself creates. `dispatch_home` is NOT in this
    /// list on purpose — `qd bootstrap` owns it, and R15 says to call into that
    /// rather than duplicate it.
    pub fn owned_dirs(&self) -> Vec<PathBuf> {
        vec![self.root.clone(), self.bin.clone(), self.state.clone()]
    }

    pub fn bin_path(&self, name: &str) -> PathBuf {
        self.bin.join(name)
    }
}

/// Which kind of install the running `qd` came from. This is the fact that
/// decides whether a missing `qw` is CHECK-AND-EXPLAIN or COPY (R15 item 2),
/// and whether the PATH step should edit an rc file at all (R15 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallChannel {
    /// Installed by Homebrew. Brew installs `qd` and `qw` side by side and puts
    /// its own `bin` on PATH, so both of those steps are normally already
    /// satisfied — setup verifies and says so instead of copying files around
    /// or editing rc files under a package manager's feet.
    Homebrew,
    /// Running out of `~/.quorum/bin` — a placed install. Same posture as
    /// Homebrew for the sibling check; the PATH block is ours to write.
    QuorumBin,
    /// Running out of `target/debug` or `target/release` — the contributor
    /// path. This is the ONLY channel where setup copies binaries.
    FromSource,
    /// Anything else (a hand-placed binary, a container image). Treated like
    /// `QuorumBin` minus the assumption about where it lives.
    Unknown,
}

impl InstallChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            InstallChannel::Homebrew => "homebrew",
            InstallChannel::QuorumBin => "quorum-bin",
            InstallChannel::FromSource => "from-source",
            InstallChannel::Unknown => "unknown",
        }
    }

    /// Does setup place `qd`/`qw` into `~/.quorum/bin` for this channel?
    /// FROM-SOURCE ONLY (R15 item 2: "Keep the copy path only for the
    /// from-source case"). Copying a Homebrew-managed binary would fork the
    /// install — two `qd`s that upgrade independently, which is exactly the
    /// version skew ADR-0020 is trying to prevent.
    pub fn places_binaries(self) -> bool {
        matches!(self, InstallChannel::FromSource)
    }
}

/// The standard Homebrew prefixes, used when `HOMEBREW_PREFIX` is not exported
/// into this process (it usually is not — `brew shellenv` sets it for shells,
/// not for every child).
const BREW_PREFIXES: &[&str] = &["/opt/homebrew", "/usr/local/Homebrew", "/home/linuxbrew/.linuxbrew"];

/// Classify the running executable. PURE: every input is an argument, so the
/// unit tests below cover all four channels with no install of any kind.
///
/// Order matters. `target/{debug,release}` is checked FIRST because a repo
/// checked out under a Homebrew prefix (or under `~/.quorum`) would otherwise
/// be misread as an installed binary and get the wrong remedy.
pub fn detect_channel(
    exe: &Path,
    layout: &QuorumLayout,
    homebrew_prefix: Option<&str>,
) -> InstallChannel {
    let dir = exe.parent().unwrap_or(exe);

    // `…/target/debug/qd` or `…/target/release/qd`.
    let from_source = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "debug" || n == "release")
        && dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("target");
    if from_source {
        return InstallChannel::FromSource;
    }

    if dir == layout.bin {
        return InstallChannel::QuorumBin;
    }

    // A brew binary is normally a symlink into `<prefix>/Cellar/<formula>/…`,
    // and `current_exe()` resolves symlinks — so the Cellar component is the
    // most reliable tell, with the prefix as the fallback for a non-Cellar
    // layout.
    if exe.components().any(|c| c.as_os_str() == "Cellar") {
        return InstallChannel::Homebrew;
    }
    let prefixes = homebrew_prefix
        .filter(|p| !p.is_empty())
        .map(|p| vec![p.to_string()])
        .unwrap_or_else(|| BREW_PREFIXES.iter().map(|s| s.to_string()).collect());
    if prefixes.iter().any(|p| exe.starts_with(p)) {
        return InstallChannel::Homebrew;
    }

    InstallChannel::Unknown
}

/// The directory that must be on `PATH` for `qd` to be typeable — and, more
/// importantly, for the `~/.claude.json` relay pin to resolve, since that pin
/// stores the BARE command `qd` and Claude Code resolves it via `PATH` at spawn
/// time (see `relay_server::register::RELAY_BARE_COMMAND`).
///
/// Under Homebrew that is brew's own `bin` (the directory the running `qd` is
/// in), not `~/.quorum/bin`: pointing PATH at an empty `~/.quorum/bin` would
/// advertise a `qd` that is not there.
pub fn path_dir_for(channel: InstallChannel, exe_dir: Option<&Path>, layout: &QuorumLayout) -> PathBuf {
    match (channel, exe_dir) {
        (InstallChannel::Homebrew, Some(d)) | (InstallChannel::Unknown, Some(d)) => d.to_path_buf(),
        _ => layout.bin.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> QuorumLayout {
        QuorumLayout::resolve(Path::new("/home/u"), None)
    }

    #[test]
    fn defaults_land_under_quorum() {
        let l = layout();
        assert_eq!(l.root, Path::new("/home/u/.quorum"));
        assert_eq!(l.bin, Path::new("/home/u/.quorum/bin"));
        assert_eq!(l.state, Path::new("/home/u/.quorum/state"));
        assert_eq!(l.dispatch_home, Path::new("/home/u/.quorum/dispatch"));
    }

    #[test]
    fn qd_home_overrides_only_the_engine_dir() {
        let l = QuorumLayout::resolve(Path::new("/home/u"), Some("/tmp/engine"));
        assert_eq!(l.dispatch_home, Path::new("/tmp/engine"));
        // bin/state still follow the root — QD_HOME moves the engine's data,
        // not the install layout (qrm's `layout_with` had the same property).
        assert_eq!(l.bin, Path::new("/home/u/.quorum/bin"));
        assert_eq!(l.state, Path::new("/home/u/.quorum/state"));
    }

    #[test]
    fn empty_qd_home_is_treated_as_unset() {
        let l = QuorumLayout::resolve(Path::new("/home/u"), Some(""));
        assert_eq!(l.dispatch_home, Path::new("/home/u/.quorum/dispatch"));
    }

    #[test]
    fn owned_dirs_excludes_the_engine_dir_qd_bootstrap_owns() {
        let l = layout();
        assert_eq!(l.owned_dirs(), vec![l.root.clone(), l.bin.clone(), l.state.clone()]);
        assert!(!l.owned_dirs().contains(&l.dispatch_home));
    }

    #[test]
    fn from_source_wins_over_every_other_shape() {
        let l = layout();
        assert_eq!(
            detect_channel(Path::new("/repo/target/debug/qd"), &l, None),
            InstallChannel::FromSource
        );
        assert_eq!(
            detect_channel(Path::new("/repo/target/release/qd"), &l, None),
            InstallChannel::FromSource
        );
        // A checkout under a brew prefix is still from-source, not Homebrew.
        assert_eq!(
            detect_channel(Path::new("/opt/homebrew/src/q/target/release/qd"), &l, None),
            InstallChannel::FromSource
        );
        // …and a `release` dir whose parent is not `target` is not from-source.
        assert_eq!(
            detect_channel(Path::new("/opt/pkg/release/qd"), &l, None),
            InstallChannel::Unknown
        );
    }

    #[test]
    fn quorum_bin_is_recognised() {
        let l = layout();
        assert_eq!(
            detect_channel(Path::new("/home/u/.quorum/bin/qd"), &l, None),
            InstallChannel::QuorumBin
        );
    }

    #[test]
    fn homebrew_via_cellar_component_or_prefix() {
        let l = layout();
        assert_eq!(
            detect_channel(
                Path::new("/opt/homebrew/Cellar/quorum-dispatch/0.1.0/bin/qd"),
                &l,
                None
            ),
            InstallChannel::Homebrew
        );
        assert_eq!(
            detect_channel(Path::new("/opt/homebrew/bin/qd"), &l, None),
            InstallChannel::Homebrew
        );
        // An explicit HOMEBREW_PREFIX replaces the standard list.
        assert_eq!(
            detect_channel(Path::new("/custom/brew/bin/qd"), &l, Some("/custom/brew")),
            InstallChannel::Homebrew
        );
        assert_eq!(
            detect_channel(Path::new("/opt/homebrew/bin/qd"), &l, Some("/custom/brew")),
            InstallChannel::Unknown
        );
    }

    #[test]
    fn only_from_source_places_binaries() {
        assert!(InstallChannel::FromSource.places_binaries());
        for c in [
            InstallChannel::Homebrew,
            InstallChannel::QuorumBin,
            InstallChannel::Unknown,
        ] {
            assert!(!c.places_binaries(), "{c:?} must never copy a binary");
        }
    }

    #[test]
    fn path_dir_follows_the_channel() {
        let l = layout();
        let brew_dir = Path::new("/opt/homebrew/bin");
        // Homebrew: brew's own bin, not an empty ~/.quorum/bin.
        assert_eq!(
            path_dir_for(InstallChannel::Homebrew, Some(brew_dir), &l),
            brew_dir
        );
        // From-source: the placement target.
        assert_eq!(
            path_dir_for(InstallChannel::FromSource, Some(Path::new("/repo/target/debug")), &l),
            l.bin
        );
        // No exe resolved at all still yields a usable answer.
        assert_eq!(path_dir_for(InstallChannel::Homebrew, None, &l), l.bin);
    }
}
