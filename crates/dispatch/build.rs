//! Build-time provenance: resolves the commit sha this `qd` was built from and
//! hands it to the crate as `QD_BUILD_SHA` (rendered by `qd --version`).
//!
//! Resolution order, most authoritative first:
//!
//! 1. `QUORUM_BUILD_SHA` — an explicit override, for a builder that knows the
//!    provenance better than this script can infer it (a release job building
//!    from an export, a reproducible rebuild pinned to a recorded sha).
//! 2. `git rev-parse HEAD` in the source tree — the truth when it is available,
//!    and it is available for the normal install path (install.sh clones and
//!    builds from a real checkout).
//! 3. `.build-sha` at the repo root — the checked-in fallback maintained by
//!    `.github/workflows/build-sha.yml`, for builds with no live `.git`
//!    (tarball, vendored tree, `cargo install --git` cache).
//! 4. `unknown` — never a build failure. A missing sha degrades `qd --version`
//!    back to the bare version string; it does not stop anyone shipping.
//!
//! The value is normalized to 12 hex chars (see `SHA_DISPLAY_LEN`): long enough
//! to be unambiguous in this repo, short enough to read in a version line.
//!
//! Prior art: `doc/tbd/versioning-spec.md` §2.3 proposes this same build stamp
//! (`build.rs` reading `git rev-parse`, honest `unknown` fallback) as the
//! instrument the deploy freeze lacks — two builds of one crate version have to
//! be discriminable. That spec is a DRAFT and is not in force, and the rest of
//! it (one workspace version, deleting the `0.1.0` fossil and its pinning test,
//! a `name version (sha date)` line on every binary) is gated on a ruling this
//! change does not pre-empt: `VERSION` is untouched here and only `qd`'s own
//! `--version` line grows the sha.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Display width of the sha. 12 hex chars, the git `--short` ceiling most tools
/// settle on for a repo this size.
const SHA_DISPLAY_LEN: usize = 12;

/// The repo root, resolved WITHOUT hard-coding how deep this crate sits inside
/// it. `dispatch/` is built from two tree shapes: nested in the monorepo
/// (`<root>/dispatch/crates/dispatch`) and as its own published repository
/// (`<root>/crates/dispatch`). A fixed `../../..` is right in exactly one of
/// them and silently overshoots the root in the other, so probe a candidate
/// ladder of depths — the same shape as `resolve_install_script`'s exe-relative
/// ladder in `src/bin/qd/verbs/bootstrap.rs` — and take the first candidate
/// carrying a root marker: a `.git` entry (a directory in a normal clone, a
/// FILE in a worktree — `exists()` accepts both) or the checked-in `.build-sha`
/// this script reads.
///
/// NEAREST-first, because the nearest enclosing checkout is the one this crate
/// belongs to — which is also exactly what `from_git`'s toplevel guard demands.
/// With no marker at any depth (a vendored copy, a `cargo install --git` cache
/// carrying neither file) fall back to the deepest candidate: `from_git` and
/// `from_file` then simply find nothing and the sha degrades to `unknown`,
/// which is this script's contract — it never fails the build.
fn repo_root(manifest: &Path) -> PathBuf {
    const LADDER: [&str; 2] = ["../..", "../../.."];
    let root = LADDER
        .iter()
        .map(|up| manifest.join(up))
        .find(|c| c.join(".git").exists() || c.join(".build-sha").is_file())
        .unwrap_or_else(|| manifest.join(LADDER[LADDER.len() - 1]));
    // `canonicalize` so the rerun-if paths printed below are the ones cargo
    // will actually stat.
    root.canonicalize().unwrap_or_else(|_| root.clone())
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = repo_root(&manifest);

    let sha = from_env()
        .or_else(|| from_git(&root))
        .or_else(|| from_file(&root))
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=QD_BUILD_SHA={sha}");
    println!("cargo:rerun-if-env-changed=QUORUM_BUILD_SHA");
    println!(
        "cargo:rerun-if-changed={}",
        root.join(".build-sha").display()
    );
    // Track HEAD (and the ref it points at) so a new commit or a branch switch
    // re-resolves the sha. This costs ONE recompile of this crate per commit —
    // deliberate: a version line that silently reports the sha from whenever the
    // crate last happened to rebuild is worse than useless, it is misleading.
    for p in head_watch_paths(&root) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
}

/// The explicit override. Empty/whitespace is treated as unset, so
/// `QUORUM_BUILD_SHA=` in an env file does not pin the version line to "".
fn from_env() -> Option<String> {
    normalize(&std::env::var("QUORUM_BUILD_SHA").ok()?)
}

/// `git rev-parse HEAD`, run in the source tree. Any failure (no git on PATH,
/// no repository, an empty repo with no commits) is a `None`, never a panic.
///
/// Guarded by a toplevel check: `git -C <dir>` walks UP, so a copy of this tree
/// vendored inside somebody else's repository would otherwise answer with THAT
/// repository's HEAD — a confidently wrong sha, which is the one outcome worse
/// than `unknown`. Unless git's toplevel IS our root, we are not in this repo's
/// checkout and the `.build-sha` fallback is the better answer.
fn from_git(root: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(["-C", root.to_str()?])
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).ok())
            .flatten()
    };
    let toplevel = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?.trim())
        .canonicalize()
        .ok()?;
    if toplevel != root.canonicalize().ok()? {
        return None;
    }
    normalize(&git(&["rev-parse", "HEAD"])?)
}

/// The checked-in fallback written by the `build-sha` workflow.
fn from_file(root: &Path) -> Option<String> {
    normalize(&std::fs::read_to_string(root.join(".build-sha")).ok()?)
}

/// Trim, reject anything that is not lowercase hex (a stray editor line, a
/// half-written file, `ref: refs/...`), and truncate to the display width.
fn normalize(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        s.chars()
            .take(SHA_DISPLAY_LEN)
            .collect::<String>()
            .to_lowercase(),
    )
}

/// The files whose content changes when HEAD moves: `.git/HEAD` always, plus
/// the ref file it names when HEAD is symbolic (the usual on-a-branch case).
/// Detached HEAD needs only `.git/HEAD` itself. Returns the paths that exist;
/// a worktree's `.git` is a FILE pointing elsewhere, and we simply watch that
/// file rather than parse it — it is rewritten on checkout too.
fn head_watch_paths(root: &Path) -> Vec<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_file() {
        return vec![dot_git];
    }
    let head = dot_git.join("HEAD");
    let Ok(contents) = std::fs::read_to_string(&head) else {
        return Vec::new();
    };
    let mut paths = vec![head];
    if let Some(r) = contents.trim().strip_prefix("ref: ") {
        let ref_path = dot_git.join(r);
        if ref_path.exists() {
            paths.push(ref_path);
        } else {
            // A packed ref has no loose file; `packed-refs` is what moves.
            let packed = dot_git.join("packed-refs");
            if packed.exists() {
                paths.push(packed);
            }
        }
    }
    paths
}
