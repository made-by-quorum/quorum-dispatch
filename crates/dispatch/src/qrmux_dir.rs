//! C1 M4 (D2): the engine's qrmux socket-dir resolver — the embedded-backend
//! analogue of [`crate::zmx_dir::resolve_zmx_dir`].
//!
//! Two tiers, NO literal `/tmp` (ADD-14: the engine never WRITES under literal
//! `/tmp`):
//!   1. `$XDG_RUNTIME_DIR/qrmux` (per-user, systemd-managed), else
//!   2. `<sbHome>/mux` where `sbHome = SB_HOME || <home>/.sb`, resolved through
//!      the existing [`SbPaths::from_home_env`] seam (paths.rs:53-58) so the mux
//!      dir MOVES with a relocated engine state dir and with SB_HOME-only jails
//!      (spec D2/R32: NOT literal `$HOME/.sb`).
//!
//! This mirrors qrmux's own standalone fallback (`server/socket.rs::resolve_socket_dir`)
//! so engine and standalone agree fully (ADR 0013). Embedded-path agreement is
//! GUARANTEED by `--socket-dir` propagation (D1/R26) regardless; this resolver is
//! the single source of truth for the dir the engine passes per-call into the
//! client ops AND propagates to the daemon.
//!
//! sun_path-length guard at resolve time (D2/R34): a Unix-domain socket path must
//! fit the platform `sockaddr_un.sun_path`; an over-long dir would make `bind()`
//! fail opaquely. We guard here with a remedy-naming error (set XDG_RUNTIME_DIR
//! or shorten SB_HOME). Gate jails set XDG_RUNTIME_DIR, so tier 1 governs jailed
//! runs and no row trips the guard.
//!
//! L9a discipline (paths.rs): the home + SB_HOME come through the injected `Env`
//! seam + an injected home, NEVER raw `std::env` — tests inject temp dirs.

use std::path::{Path, PathBuf};

use crate::effects::Env;
use crate::paths::SbPaths;

/// Conservative upper bound on the bytes the socket *filename* + NUL add to the
/// socket-directory path. Mirrors qrmux's `SOCKET_LEAF_BUDGET` (the daemon places
/// per-session `<name>.sock`/`<name>.lock` plus `retach.log` in the dir; 16 leaves
/// headroom + NUL for the bounded `.sock`/`.lock` suffixes — the dynamic name-length
/// budget is enforced separately by qrmux's `session_socket_path_for`).
const SOCKET_LEAF_BUDGET: usize = 16;

/// Platform `sun_path` capacity. macOS gives 104, Linux 108; we use the smaller
/// so a path that resolves on macOS also resolves on Linux. Mirrors
/// qrmux::server::socket::SUN_PATH_MAX.
const SUN_PATH_MAX: usize = 104;

/// Resolve the engine's qrmux socket dir from the injected `home` + `Env` seam.
///
/// Tier 1 `$XDG_RUNTIME_DIR/qrmux` (trimmed; empty/whitespace-only treated as
/// unset, matching the zmx-dir tier semantics), else tier 2 `<sbHome>/mux` via
/// [`SbPaths::from_home_env`] (`state_dir = <sbHome>/state`, so the mux sibling
/// is `<sbHome>/mux`). Applies the sun_path guard with a remedy-naming error.
///
/// Returns `Err(msg)` ONLY when the guard trips; tier resolution itself is
/// total (home is always injected). The caller maps the message to an io::Error.
pub fn resolve_qrmux_dir(home: &Path, env: &dyn Env) -> Result<PathBuf, String> {
    let dir = match nonblank(env.var("XDG_RUNTIME_DIR")) {
        // Tier 1: $XDG_RUNTIME_DIR/qrmux.
        Some(xdg) => PathBuf::from(xdg.trim()).join("qrmux"),
        // Tier 2: <sbHome>/mux. SbPaths.state_dir == <sbHome>/state, so the mux
        // dir is its sibling `<sbHome>/mux`. Derive sbHome from state_dir's parent
        // so the ONE SB_HOME-resolution seam (paths.rs) governs both.
        None => {
            let paths = SbPaths::from_home_env(home, env);
            let sb_home = paths
                .state_dir
                .parent()
                .map(Path::to_path_buf)
                // state_dir is always `<sbHome>/state`, so parent is always Some;
                // the fallback is unreachable but keeps this total.
                .unwrap_or_else(|| home.join(".quorum").join("dispatch"));
            sb_home.join("mux")
        }
    };

    // sun_path guard: per-session sockets bind against `<dir>/<name>.sock`, so the
    // dir must leave room for the longest leaf. This is a conservative DIR-level
    // pre-check using a fixed leaf budget; the authoritative DYNAMIC per-session
    // budget (`len(dir) + 1 + len(name) + 5 ≤ 104`) lives in qrmux's
    // `session_socket_path_for`, which owns the real `<name>.sock` check.
    let projected = dir.as_os_str().len() + SOCKET_LEAF_BUDGET;
    if projected > SUN_PATH_MAX {
        return Err(format!(
            "qrmux socket dir {:?} is too long ({} bytes; the Unix socket path must fit {} \
             bytes): set XDG_RUNTIME_DIR to a short per-user runtime dir, or shorten SB_HOME",
            dir, projected, SUN_PATH_MAX,
        ));
    }
    Ok(dir)
}

/// `env.X && env.X.trim()`: Some only when non-empty after trimming (the zmx-dir
/// tier convention; an empty/whitespace-only XDG_RUNTIME_DIR is treated as unset).
fn nonblank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;

    fn env(uid: u32, pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            uid,
        }
    }

    // --- tier matrix ---

    #[test]
    fn tier1_xdg_runtime_dir_wins() {
        let e = env(501, &[("XDG_RUNTIME_DIR", "/run/user/501")]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/run/user/501/qrmux"));
    }

    #[test]
    fn tier1_xdg_trimmed() {
        // Whitespace-padded XDG value → trimmed, then joined with `qrmux`.
        let e = env(501, &[("XDG_RUNTIME_DIR", "  /run/user/501  ")]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/run/user/501/qrmux"));
    }

    #[test]
    fn tier2_sb_home_default_under_home() {
        // No XDG, no SB_HOME → <home>/.quorum/dispatch/mux.
        let e = env(501, &[]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/jail/home/.quorum/dispatch/mux"));
    }

    #[test]
    fn tier2_relocates_with_sb_home() {
        // SB_HOME-relocation (R32): the mux dir MOVES with the relocated engine
        // state dir, NOT pinned to literal $HOME/.sb.
        let e = env(501, &[("SB_HOME", "/elsewhere/sbdata")]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/elsewhere/sbdata/mux"));
    }

    #[test]
    fn tier2_sb_home_only_jail() {
        // An SB_HOME-only jail (HOME points at a real-ish home, but SB_HOME
        // relocates state) still moves the mux dir under SB_HOME.
        let e = env(501, &[("SB_HOME", "/jail/sb")]);
        let dir = resolve_qrmux_dir(Path::new("/real/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/jail/sb/mux"));
    }

    #[test]
    fn empty_xdg_falls_through_to_tier2() {
        // Empty/whitespace-only XDG counts as unset → tier 2.
        let e = env(501, &[("XDG_RUNTIME_DIR", "   ")]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/jail/home/.quorum/dispatch/mux"));
    }

    #[test]
    fn empty_sb_home_falls_back_to_default() {
        // SB_HOME="" is falsy (JS `||`) → default <home>/.quorum/dispatch/mux (paths.rs seam).
        let e = env(501, &[("SB_HOME", "")]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert_eq!(dir, PathBuf::from("/jail/home/.quorum/dispatch/mux"));
    }

    #[test]
    fn no_literal_tmp_tier_anywhere() {
        // ADD-14: the engine never resolves a literal /tmp tier. Even with NO env
        // set, tier 2 lands under the injected home, not /tmp.
        let e = env(501, &[]);
        let dir = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap();
        assert!(
            !dir.to_string_lossy().starts_with("/tmp/"),
            "no /tmp tier: {dir:?}"
        );
    }

    // --- sun_path guard trip ---

    #[test]
    fn guard_trips_on_overlong_tier2() {
        // A home root long enough that <root>/.sb/mux + leaf budget > 104.
        let long_home = format!("/home/{}", "x".repeat(120));
        let e = env(501, &[]);
        let err = resolve_qrmux_dir(Path::new(&long_home), &e).unwrap_err();
        assert!(
            err.contains("too long") && err.contains("XDG_RUNTIME_DIR") && err.contains("SB_HOME"),
            "guard error must name the remedy, got: {err}"
        );
    }

    #[test]
    fn guard_trips_on_overlong_xdg() {
        let long_xdg = format!("/run/{}", "y".repeat(120));
        let e = env(501, &[("XDG_RUNTIME_DIR", &long_xdg)]);
        let err = resolve_qrmux_dir(Path::new("/jail/home"), &e).unwrap_err();
        assert!(err.contains("too long"), "got: {err}");
    }

    #[test]
    fn short_xdg_does_not_trip_guard() {
        let e = env(501, &[("XDG_RUNTIME_DIR", "/run/user/501")]);
        assert!(resolve_qrmux_dir(Path::new("/jail/home"), &e).is_ok());
    }
}
