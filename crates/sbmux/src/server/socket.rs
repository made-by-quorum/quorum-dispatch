use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Conservative upper bound on the bytes the socket *filename* can add to the
/// socket directory path. The longest filename we place in the dir is
/// `sbmux.sock` (10 bytes); we round up to leave headroom for `sbmux.lock`,
/// `retach.log`, and a NUL terminator.
const SOCKET_LEAF_BUDGET: usize = 16;

/// Platform `sun_path` capacity for a `sockaddr_un`. macOS gives 104, Linux 108;
/// we use the smaller so a path that resolves on macOS also resolves on Linux.
/// A bind() against an over-long path fails opaquely ("invalid argument" /
/// "name too long"), so we guard at resolve time with a remedy-naming error.
const SUN_PATH_MAX: usize = 104;

/// Bytes the `.sock` leaf suffix adds beyond the session name itself, for the
/// per-session budget check (WS-C §2). The leaf is `<name>.sock` → `<name>` plus
/// 5 suffix bytes; the dynamic budget is `len(dir) + 1 (path sep) + len(name) + 5`.
const SOCK_SUFFIX_LEN: usize = 5; // ".sock"

/// Socket directory with proper permissions (0o700, created atomically).
///
/// Two tiers, NO literal `/tmp` (ADD-14; checkpoint rider R-B):
/// 1. `$XDG_RUNTIME_DIR/sbmux` (per-user, mode 0700, systemd-managed), else
/// 2. `<sbHome>/mux` where `sbHome = $SB_HOME || $HOME/.quorum/dispatch`.
///
/// **SB_HOME is honored** (implementer choice, recommended by the spec and named
/// in ADR 0008): the standalone fallback mirrors the engine's `resolve_sbmux_dir`
/// so a relocated engine state dir or an SB_HOME-only jail moves the mux dir too,
/// and engine + standalone agree fully. Falls back to `$HOME/.quorum/dispatch` only when
/// SB_HOME is unset, and to the user's home dir when HOME is unset as well.
pub fn socket_dir() -> anyhow::Result<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from);
    let sb_home = std::env::var("SB_HOME").ok().map(PathBuf::from);
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    socket_dir_impl_env(runtime_dir.as_deref(), sb_home.as_deref(), home.as_deref())
}

/// Resolve the socket directory *path* from explicit env inputs, applying the
/// two-tier policy above and the sun_path-length guard, WITHOUT creating it.
/// Pure (no filesystem, no process env) so it is unit-testable.
fn resolve_socket_dir(
    runtime_dir: Option<&Path>,
    sb_home: Option<&Path>,
    home: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let dir = if let Some(xdg) = runtime_dir {
        xdg.join("sbmux")
    } else {
        // Tier 2: <sbHome>/mux, sbHome = SB_HOME || HOME/.quorum/dispatch.
        let sb_home: PathBuf = if let Some(sh) = sb_home {
            sh.to_path_buf()
        } else if let Some(h) = home {
            h.join(".quorum").join("dispatch")
        } else {
            anyhow::bail!(
                "cannot resolve sbmux socket dir: neither XDG_RUNTIME_DIR, SB_HOME, nor HOME is set"
            );
        };
        sb_home.join("mux")
    };

    // sun_path guard: bind() against `<dir>/sbmux.sock` must fit the platform
    // sockaddr_un. Name the remedy so the failure is actionable, not opaque.
    let projected = dir.as_os_str().len() + SOCKET_LEAF_BUDGET;
    if projected > SUN_PATH_MAX {
        anyhow::bail!(
            "sbmux socket dir {:?} is too long ({} bytes; the Unix socket path must fit {} bytes): \
             set XDG_RUNTIME_DIR to a short per-user runtime dir, or shorten SB_HOME/HOME",
            dir,
            projected,
            SUN_PATH_MAX,
        );
    }
    Ok(dir)
}

/// Resolve via env inputs, then create the directory with 0o700 and the same
/// symlink/ownership/permission belts as before.
fn socket_dir_impl_env(
    runtime_dir: Option<&Path>,
    sb_home: Option<&Path>,
    home: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let dir = resolve_socket_dir(runtime_dir, sb_home, home)?;
    create_socket_dir(&dir)?;
    Ok(dir)
}

/// Test/back-compat shim: resolve+create using only an XDG override, deriving
/// SB_HOME/HOME from the process env for tier 2. Used by the symlink/permission
/// tests, which exercise tier 1 with an explicit override.
#[cfg(test)]
fn socket_dir_impl(runtime_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    let sb_home = std::env::var("SB_HOME").ok().map(PathBuf::from);
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    socket_dir_impl_env(runtime_dir, sb_home.as_deref(), home.as_deref())
}

/// Create the socket directory with 0o700 atomically and run the symlink /
/// ownership / permission belts. Factored out so resolution stays pure.
fn create_socket_dir(dir: &Path) -> anyhow::Result<()> {
    let uid = nix::unistd::getuid();
    // Create directory with 0o700 atomically — no TOCTOU window
    if let Some(parent) = dir.parent() {
        // Tier-2 (<sbHome>/mux) parent may not exist yet; XDG tier-1 parent
        // always does. Best-effort create of the parent chain so the final
        // 0o700 create can succeed.
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Use symlink_metadata (lstat) to detect symlinks — metadata() follows them
            let meta = std::fs::symlink_metadata(dir)
                .map_err(|e| anyhow::anyhow!("cannot stat socket directory {dir:?}: {e}"))?;
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "socket directory {dir:?} is a symlink — possible symlink attack, refusing to start"
                );
            }
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != uid.as_raw() {
                anyhow::bail!(
                    "socket directory {dir:?} owned by uid {} (expected {}) — possible attack",
                    meta.uid(),
                    uid.as_raw(),
                );
            }
            // Fix permissions if they drifted
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o777 != 0o700 {
                if let Err(e) =
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                {
                    tracing::warn!(error = %e, "failed to fix socket directory permissions");
                }
            }
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to create socket directory {dir:?}: {e}"
            ));
        }
    }
    Ok(())
}

// ===========================================================================
// Override seam (C1 D1): the engine resolves the sbmux dir itself and passes it
// in per-call so the embedded mux and the daemon agree. `Some(dir)` uses the
// caller-supplied directory verbatim (after creation + belts); `None` falls
// back to the env-derived two-tier policy above. The no-arg public fns are
// `None`-passing wrappers, so standalone-CLI behavior is unchanged.
// ===========================================================================

/// Resolve the socket directory, honoring an explicit override.
/// `Some(dir)` → that exact directory (created + belted); `None` → env tiers.
pub fn socket_dir_for(dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    match dir {
        Some(d) => {
            // Caller (engine) already chose the dir; still apply the creation
            // belts and the sun_path guard so a bogus override fails loudly.
            let projected = d.as_os_str().len() + SOCKET_LEAF_BUDGET;
            if projected > SUN_PATH_MAX {
                anyhow::bail!(
                    "sbmux socket dir {:?} is too long ({} bytes; the Unix socket path must fit \
                     {} bytes): set XDG_RUNTIME_DIR to a short per-user runtime dir, or shorten \
                     SB_HOME/HOME",
                    d,
                    projected,
                    SUN_PATH_MAX,
                );
            }
            create_socket_dir(d)?;
            Ok(d.to_path_buf())
        }
        None => socket_dir(),
    }
}

// WS-C M3b: the legacy shared-socket path helpers (`socket_path[_for]`,
// `lock_path[_for]`, which computed `<dir>/sbmux.sock` / `<dir>/sbmux.lock`) are
// RETIRED (spec §1, §9). Per-session callers use `session_socket_path_for` /
// `session_lock_path_for`; the ONLY remaining reference to the `sbmux.sock` NAME
// is the §5.3 legacy-visibility probe (`discovery::LEGACY_LEAF`). Nothing binds
// or expects the shared leaf anymore.

// ===========================================================================
// WS-C M2: per-session naming (§2). In SINGLE-SESSION mode the daemon binds
// `<dir>/<name>.sock` (one socket per session) instead of the legacy shared
// `<dir>/sbmux.sock`. The dir resolution is UNCHANGED (no third scheme,
// R-1(1)); only the leaf changes. M3 retires the legacy leaf when the engine
// flips to per-session launch+discovery.
// ===========================================================================

/// Reserved session names that must never become socket leaves (WS-C §2 tightening,
/// W-1 named divergence). `sbmux` would collide with the legacy `sbmux.sock` leaf;
/// a leading `.` would create a dotfile leaf. Both are accepted by the wire-level
/// [`crate::session::validate_session_name`] (mid-name `.` stays legal there for the
/// events-filename parse) — the tightening lives HERE, on the `--session` identity
/// the daemon binds a socket for.
const RESERVED_SESSION_NAME: &str = "sbmux";

/// The fixed leaf STEM all sessions collapse onto under the G-ISOL negative-control
/// seam (see [`shared_fate_test_mode`]): `<dir>/shared.sock` / `<dir>/shared.lock`.
const SHARED_FATE_LEAF_STEM: &str = "shared";

/// G-ISOL NEGATIVE-CONTROL seam (gate teeth, spec §7). When the process env has
/// `SBMUX_TEST_SHARED=1`, the per-session daemon split is DELIBERATELY collapsed
/// into the retired shared-fate world so the inversion's negative control is
/// CONSTRUCTABLE (red-team M4): (a) socket/lock naming collapses to the single
/// fixed `shared.{sock,lock}` leaf in BOTH the daemon bind AND the launcher/client
/// connect path ([`session_socket_path_for`] / [`session_lock_path_for`]), and (b)
/// the daemon's capacity-1 identity gate accepts ANY session name (the manager goes
/// multi-session — the machinery still exists; see
/// `client_handler::DaemonCtx::check_identity`) AND the client-side ServerHello
/// identity belt is relaxed (`client::session_client::session_handshake`). The
/// net effect: both `sb new A` and `sb new B` provably reach ONE daemon process on
/// `shared.sock`, so SIGKILLing it kills BOTH — the shared-fate RED the G-ISOL
/// negative control detects.
///
/// Same env-seam discipline as the `RETACH_B1_BREAK` breaker (`session.rs`): NEVER
/// set by any production path; documented in code; inert (returns `false`) when the
/// env is unset, so every production caller computes the real per-session leaf.
pub fn shared_fate_test_mode() -> bool {
    std::env::var("SBMUX_TEST_SHARED").ok().as_deref() == Some("1")
}

/// Validate the `--session` identity for SINGLE-SESSION mode (WS-C §2). Layers the
/// W-1 tightening on top of the engine's S2 charset belt:
///
/// 1. the existing wire belt ([`crate::session::validate_session_name`]: non-empty,
///    ≤128B, charset `[a-zA-Z0-9_.-]`), then
/// 2. REFUSE the reserved `sbmux` leaf (legacy-socket collision), and
/// 3. REFUSE leading-`.` names (dotfile leaf).
///
/// Refuse-don't-escape: names outside the charset are rejected, never rewritten, so
/// the name→leaf mapping stays injective (§2). The dynamic sun_path budget is a
/// SEPARATE check ([`session_socket_path_for`]) because it depends on the resolved dir.
pub fn validate_session_identity(name: &str) -> anyhow::Result<()> {
    crate::session::validate_session_name(name)?;
    if name == RESERVED_SESSION_NAME {
        anyhow::bail!(
            "session name '{}' is reserved (collides with the legacy shared socket leaf)",
            RESERVED_SESSION_NAME
        );
    }
    if name.starts_with('.') {
        anyhow::bail!(
            "session name '{}' must not start with '.' (dotfile socket leaf)",
            name
        );
    }
    Ok(())
}

/// Maximum session-name length that fits the sun_path budget FOR THIS DIR
/// (`SUN_PATH_MAX - len(dir) - 1 (sep) - 5 (".sock")`). Saturating so a dir that
/// is already over budget yields 0, not an underflow panic.
fn max_session_name_len_for(dir: &Path) -> usize {
    SUN_PATH_MAX
        .saturating_sub(dir.as_os_str().len())
        .saturating_sub(1) // path separator
        .saturating_sub(SOCK_SUFFIX_LEN)
}

/// Resolve the per-session socket path `<dir>/<name>.sock`, honoring the socket-dir
/// override (`None` → env tiers) and enforcing the DYNAMIC sun_path budget (§2):
/// `len(dir) + 1 + len(name) + 5 ≤ 104`. The remedy-naming error states the max name
/// length FOR THIS DIR and the XDG/SB_HOME remedy (zmx printSessionNameTooLong
/// precedent). The caller is expected to have already run [`validate_session_identity`].
pub fn session_socket_path_for(dir: Option<&Path>, name: &str) -> anyhow::Result<PathBuf> {
    let resolved = socket_dir_for(dir)?;
    // G-ISOL negative-control seam: collapse ALL names onto one fixed leaf so both
    // sessions provably land on ONE daemon's socket (the shared-fate RED). Inert in
    // production (env unset). Budget check is moot for the short fixed stem.
    if shared_fate_test_mode() {
        return Ok(resolved.join(format!("{SHARED_FATE_LEAF_STEM}.sock")));
    }
    let max = max_session_name_len_for(&resolved);
    if name.len() > max {
        anyhow::bail!(
            "session name too long ({} bytes, max {} for socket dir {:?}); \
             set XDG_RUNTIME_DIR to a short runtime dir or shorten SB_HOME",
            name.len(),
            max,
            resolved,
        );
    }
    Ok(resolved.join(format!("{name}.sock")))
}

/// Per-session launch lock `<dir>/<name>.lock` (§2). M2 binds the socket; the
/// launcher (M3) holds this flock across spawn. Provided here so the naming is
/// defined in one place.
pub fn session_lock_path_for(dir: Option<&Path>, name: &str) -> anyhow::Result<PathBuf> {
    let resolved = socket_dir_for(dir)?;
    // G-ISOL negative-control seam: same-leaf collapse as the socket path so racing
    // same-fate creators serialize on ONE lock (inert in production).
    if shared_fate_test_mode() {
        return Ok(resolved.join(format!("{SHARED_FATE_LEAF_STEM}.lock")));
    }
    Ok(resolved.join(format!("{name}.lock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// De-/tmp'd fallback (checkpoint rider R-B): tier 1 = `$XDG_RUNTIME_DIR/sbmux`,
    /// tier 2 = `<sbHome>/mux` (`sbHome = SB_HOME || $HOME/.quorum/dispatch`). The literal
    /// `/tmp/sbmux-{uid}` fallback is GONE (ADD-14). Asserted via the pure
    /// resolver so it doesn't depend on the runner's process env.
    #[test]
    fn socket_dir_returns_correct_format() {
        // Tier 1: XDG present.
        let xdg = Path::new("/run/user/1000");
        let dir = resolve_socket_dir(Some(xdg), None, None).unwrap();
        assert_eq!(dir, xdg.join("sbmux"));

        // Tier 2a: no XDG, SB_HOME present → <SB_HOME>/mux.
        let sb_home = Path::new("/home/u/.sb");
        let dir = resolve_socket_dir(None, Some(sb_home), Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, sb_home.join("mux"));

        // Tier 2b: no XDG, no SB_HOME, HOME present → <HOME>/.quorum/dispatch/mux.
        let dir = resolve_socket_dir(None, None, Some(Path::new("/home/u"))).unwrap();
        assert_eq!(dir, Path::new("/home/u/.quorum/dispatch/mux"));

        // No literal /tmp tier anywhere.
        for d in [
            resolve_socket_dir(Some(xdg), None, None).unwrap(),
            resolve_socket_dir(None, Some(sb_home), None).unwrap(),
        ] {
            assert!(
                !d.to_string_lossy().starts_with("/tmp/"),
                "fallback must not be under literal /tmp: {:?}",
                d
            );
        }
    }

    /// SB_HOME wins over HOME for tier 2 (engine-mirroring choice, ADR 0008).
    #[test]
    fn socket_dir_honors_sb_home_over_home() {
        let dir = resolve_socket_dir(
            None,
            Some(Path::new("/relocated/state")),
            Some(Path::new("/home/u")),
        )
        .unwrap();
        assert_eq!(dir, Path::new("/relocated/state/mux"));
    }

    /// Neither XDG, SB_HOME, nor HOME → a named error, never a panic.
    #[test]
    fn socket_dir_errors_when_nothing_resolves() {
        let err = resolve_socket_dir(None, None, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("XDG_RUNTIME_DIR") && err.contains("SB_HOME") && err.contains("HOME"),
            "error should name the env remedies, got: {err}"
        );
    }

    /// sun_path guard: an over-long resolved dir trips with a remedy-naming error.
    #[test]
    fn socket_dir_sun_path_guard_trips_on_overlong_root() {
        // A home root long enough that <root>/.quorum/dispatch/mux + sbmux.sock > 104 bytes.
        let long_root = format!("/home/{}", "x".repeat(120));
        let err = resolve_socket_dir(None, None, Some(Path::new(&long_root)))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("too long") && err.contains("XDG_RUNTIME_DIR") && err.contains("SB_HOME"),
            "guard error should name the remedy, got: {err}"
        );
    }

    /// sun_path guard also fires on an explicit override (engine-supplied dir).
    #[test]
    fn socket_dir_for_override_sun_path_guard_trips() {
        let long_dir = format!("/run/{}", "y".repeat(120));
        let err = socket_dir_for(Some(Path::new(&long_dir)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("too long"), "got: {err}");
    }

    /// A short explicit override resolves to itself (the agreement mechanism:
    /// engine passes its resolved dir straight through). WS-C M3b: the per-session
    /// leaf forms are what the engine/launcher use now (the legacy shared
    /// `socket_path_for`/`lock_path_for` were retired).
    #[test]
    fn socket_dir_for_override_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ovr");
        let got = socket_dir_for(Some(&dir)).unwrap();
        assert_eq!(got, dir);
        assert!(got.is_dir());
        assert_eq!(
            session_socket_path_for(Some(&dir), "alpha").unwrap(),
            dir.join("alpha.sock")
        );
        assert_eq!(
            session_lock_path_for(Some(&dir), "alpha").unwrap(),
            dir.join("alpha.lock")
        );
    }

    #[test]
    fn socket_dir_creates_directory() {
        let dir = socket_dir().unwrap();
        assert!(
            dir.exists(),
            "socket_dir() should create the directory at {:?}",
            dir
        );
        assert!(
            dir.is_dir(),
            "socket_dir() path should be a directory, not a file"
        );
    }

    #[test]
    fn socket_dir_has_correct_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = socket_dir().unwrap();
        let meta = std::fs::metadata(&dir).expect("should be able to stat socket directory");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "socket directory should have mode 0o700, got: {:#o}",
            mode
        );
    }

    #[test]
    fn socket_dir_idempotent() {
        let first = socket_dir().unwrap();
        let second = socket_dir().unwrap();
        assert_eq!(
            first, second,
            "calling socket_dir() twice should return the same path"
        );
        assert!(
            second.exists(),
            "directory should still exist after second call"
        );
    }

    #[test]
    fn socket_dir_rejects_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        // Renamed retach → sbmux with the socket-layout port (spec ground rule 3).
        let sym_dir = tmp.path().join("sbmux");
        std::fs::create_dir(&real_dir).unwrap();
        std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();

        // Use socket_dir_impl with an explicit override instead of mutating
        // the process environment, which is unsound in multi-threaded test runners.
        let result = socket_dir_impl(Some(tmp.path()));

        assert!(result.is_err(), "should reject symlink socket directory");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("symlink"),
            "error should mention symlink: {}",
            err
        );
    }

    #[test]
    fn socket_dir_repairs_wrong_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        // Renamed retach → sbmux with the socket-layout port (spec ground rule 3).
        let dir = tmp.path().join("sbmux");
        std::fs::DirBuilder::new().mode(0o755).create(&dir).unwrap();

        // Use socket_dir_impl with an explicit override instead of mutating
        // the process environment, which is unsound in multi-threaded test runners.
        let result = socket_dir_impl(Some(tmp.path()));

        assert!(result.is_ok(), "should succeed and repair permissions");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "permissions should be repaired to 0o700, got: {:#o}",
            mode
        );
    }

    // ===================================================================
    // WS-C M2: per-session naming + identity validation (§2).
    // ===================================================================

    /// Single-session socket leaf is exactly `<name>.sock`, and the lock leaf
    /// `<name>.lock`, both in the resolved dir (override round-trips).
    #[test]
    fn session_socket_leaf_is_name_dot_sock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mux");
        let sock = session_socket_path_for(Some(&dir), "alpha").unwrap();
        assert_eq!(sock, dir.join("alpha.sock"));
        assert_eq!(sock.file_name().unwrap(), "alpha.sock");
        let lock = session_lock_path_for(Some(&dir), "alpha").unwrap();
        assert_eq!(lock, dir.join("alpha.lock"));
    }

    /// The dynamic per-session budget error names the COMPUTED max length for
    /// this dir AND the XDG/SB_HOME remedy (exact prefix + computed max — house
    /// exact-equality lesson on the contractual fragments).
    #[test]
    fn session_socket_budget_violation_remedy_naming() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mux");
        std::fs::create_dir_all(&dir).unwrap();
        // Compute the exact max for THIS dir, then exceed it by one byte.
        let max = max_session_name_len_for(&dir);
        let name = "n".repeat(max + 1);
        let err = session_socket_path_for(Some(&dir), &name)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(&format!(
                "session name too long ({} bytes, max {} for socket dir",
                name.len(),
                max
            )),
            "exact remedy-naming prefix with computed max, got: {err}"
        );
        assert!(
            err.contains("set XDG_RUNTIME_DIR to a short runtime dir or shorten SB_HOME"),
            "remedy clause, got: {err}"
        );
    }

    /// A name exactly AT the computed max resolves (boundary is inclusive).
    #[test]
    fn session_socket_budget_boundary_inclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mux");
        std::fs::create_dir_all(&dir).unwrap();
        let max = max_session_name_len_for(&dir);
        // Cap at 128B (the charset belt) so we test the budget, not the belt.
        let n = max.min(120);
        let name = "n".repeat(n);
        let sock = session_socket_path_for(Some(&dir), &name).unwrap();
        assert_eq!(sock, dir.join(format!("{name}.sock")));
    }

    /// `--session` identity validation: reserved `sbmux` and leading-`.` are
    /// REFUSED (W-1 tightening), while mid-name `.` and the S2 charset stay legal.
    #[test]
    fn validate_session_identity_tightening() {
        // Accepts ordinary names incl. mid-name dots.
        assert!(validate_session_identity("my-session.1_OK").is_ok());
        assert!(validate_session_identity("a").is_ok());

        // Refuses the reserved legacy leaf.
        let err = validate_session_identity("sbmux").unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");

        // Refuses leading-dot (dotfile leaf).
        let err = validate_session_identity(".hidden")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not start with '.'"), "got: {err}");

        // Still inherits the wire belt: bad charset + empty + too-long.
        assert!(validate_session_identity("foo/bar").is_err());
        assert!(validate_session_identity("").is_err());
        assert!(validate_session_identity(&"x".repeat(129)).is_err());
    }
}
