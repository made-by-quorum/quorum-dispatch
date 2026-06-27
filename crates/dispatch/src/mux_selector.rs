//! C1 M4 (D3): QD_MUX backend selection.
//!
//! ONE parse of `QD_MUX` ([`parse_backend`]) feeds BOTH the runtime mux selection
//! ([`select_mux`]) AND the gather/`MuxDirs` dir-resolution lane (join.rs) — no
//! divergent double-read of the env var (spec item 4).
//!
//! Selection policy (spec D3):
//!   - unset / `"embedded"` → [`crate::embedded_mux::EmbeddedMux`] (the default flip)
//!   - `"zmx"` → [`crate::zmx_mux::ZmxMux`] over `RealExec` (the escape hatch)
//!   - anything else → a LOUD named error (distinct exit code + a message listing
//!     the valid values) — NEVER a silent fallback (spec G-SEL / G-NEG teeth).
//!
//! `registry.backend` is NOT written in C1 (spec D3/R9): the engine does not own
//! the live registry row, so there is no honest write seam. Listing is already
//! backend-scoped by WHICH mux runs (the whole-world rule, D4), so the field stays
//! non-load-bearing. Deferred to C2/A6 (ADR 0013).

use std::path::Path;

use crate::effects::Env;
use crate::embedded_mux::EmbeddedMux;
use crate::exec::RealExec;
use crate::mux::Mux;
use crate::zmx_mux::ZmxMux;

/// The selected mux backend, parsed ONCE from `QD_MUX`. The gather/`MuxDirs`
/// dir-resolution lane keys off this same value (single source of truth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Default: the embedded qrmux daemon (the C1 flip).
    Embedded,
    /// The zmx escape hatch (`QD_MUX=zmx`).
    Zmx,
}

/// Exit code for an invalid `QD_MUX` value. Distinct from the generic `1` so the
/// G-SEL/G-NEG negative arm can assert a specific selector-misconfig code (it
/// reuses the `codes` convention: a config/usage error class). Surfaced by the
/// call sites that print the error message and return this code.
pub const QD_MUX_INVALID_EXIT: i32 = 2;

/// The error a bogus `QD_MUX` produces: a message + the distinct exit code.
/// `real_mux` returns this so the printer call sites surface it uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorError {
    pub message: String,
    pub exit_code: i32,
}

/// Parse `QD_MUX` into a [`Backend`]. unset/empty/`"embedded"` → Embedded;
/// `"zmx"` → Zmx; anything else → a loud named [`SelectorError`].
///
/// The value is trimmed and compared case-sensitively to the documented tokens
/// (the env contract is exact-match; a stray `"Zmx"` is a misconfig, not a guess).
pub fn parse_backend(env: &dyn Env) -> Result<Backend, SelectorError> {
    match env.var("QD_MUX").as_deref().map(str::trim) {
        None | Some("") | Some("embedded") => Ok(Backend::Embedded),
        Some("zmx") => Ok(Backend::Zmx),
        Some(other) => Err(SelectorError {
            message: format!(
                "qd: invalid QD_MUX value {other:?} — valid values are \"embedded\" (default) \
                 or \"zmx\""
            ),
            exit_code: QD_MUX_INVALID_EXIT,
        }),
    }
}

/// Build the selected mux as a `Box<dyn Mux>`, resolving the qrmux dir for the
/// embedded backend from the injected `home` + `Env`. The zmx lane is dir-agnostic
/// (its dir is pinned per-call by the gather/kill/attach sites), so `home` is
/// unused there.
///
/// `Err(SelectorError)` propagates the bogus-value error to the printer call site.
pub fn select_mux(
    backend: Backend,
    home: &Path,
    env: &dyn Env,
) -> Result<Box<dyn Mux>, SelectorError> {
    match backend {
        Backend::Embedded => Ok(Box::new(EmbeddedMux::new(
            home.to_path_buf(),
            env_snapshot(env),
        ))),
        Backend::Zmx => Ok(Box::new(ZmxMux::new(RealExec))),
    }
}

/// Snapshot the env values the embedded mux needs at construction time. The
/// adapter resolves its dir lazily on first op (a session-shared daemon), but it
/// must capture HOME + the QD_HOME/XDG seam values up front because it does not
/// hold the `&dyn Env` (which is borrowed). We snapshot the two it consults.
fn env_snapshot(env: &dyn Env) -> EmbeddedEnv {
    EmbeddedEnv {
        xdg_runtime_dir: env.var("XDG_RUNTIME_DIR"),
        sb_home: env.var("QD_HOME"),
        uid: env.uid(),
    }
}

/// Minimal env snapshot the [`EmbeddedMux`] carries (it can't hold a borrowed
/// `&dyn Env`). Implements [`Env`] so the adapter can reuse
/// [`crate::qrmux_dir::resolve_qrmux_dir`] unchanged.
#[derive(Debug, Clone)]
pub struct EmbeddedEnv {
    pub xdg_runtime_dir: Option<String>,
    pub sb_home: Option<String>,
    pub uid: u32,
}

impl Env for EmbeddedEnv {
    fn var(&self, key: &str) -> Option<String> {
        match key {
            "XDG_RUNTIME_DIR" => self.xdg_runtime_dir.clone(),
            "QD_HOME" => self.sb_home.clone(),
            _ => None,
        }
    }
    fn uid(&self) -> u32 {
        self.uid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;

    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            uid: 501,
        }
    }

    #[test]
    fn unset_defaults_to_embedded() {
        assert_eq!(parse_backend(&env(&[])).unwrap(), Backend::Embedded);
    }

    #[test]
    fn empty_defaults_to_embedded() {
        assert_eq!(
            parse_backend(&env(&[("QD_MUX", "")])).unwrap(),
            Backend::Embedded
        );
        assert_eq!(
            parse_backend(&env(&[("QD_MUX", "   ")])).unwrap(),
            Backend::Embedded
        );
    }

    #[test]
    fn explicit_embedded() {
        assert_eq!(
            parse_backend(&env(&[("QD_MUX", "embedded")])).unwrap(),
            Backend::Embedded
        );
        // trimmed.
        assert_eq!(
            parse_backend(&env(&[("QD_MUX", "  embedded  ")])).unwrap(),
            Backend::Embedded
        );
    }

    #[test]
    fn explicit_zmx() {
        assert_eq!(
            parse_backend(&env(&[("QD_MUX", "zmx")])).unwrap(),
            Backend::Zmx
        );
    }

    #[test]
    fn bogus_value_is_loud_named_error_with_distinct_code() {
        let err = parse_backend(&env(&[("QD_MUX", "bogus")])).unwrap_err();
        assert_eq!(err.exit_code, QD_MUX_INVALID_EXIT);
        assert_ne!(err.exit_code, 1, "distinct from the generic exit 1");
        // The message names the value AND lists the valid set (G-SEL assertion).
        assert!(
            err.message.contains("bogus"),
            "names the bad value: {}",
            err.message
        );
        assert!(
            err.message.contains("embedded"),
            "lists embedded: {}",
            err.message
        );
        assert!(err.message.contains("zmx"), "lists zmx: {}", err.message);
    }

    #[test]
    fn case_sensitive_no_silent_guess() {
        // "Zmx"/"EMBEDDED" are misconfigs, not silent guesses (exact-match contract).
        assert!(parse_backend(&env(&[("QD_MUX", "Zmx")])).is_err());
        assert!(parse_backend(&env(&[("QD_MUX", "EMBEDDED")])).is_err());
    }

    #[test]
    fn select_mux_builds_both_lanes() {
        // Both lanes build a Box<dyn Mux> without panicking (the embedded one
        // resolves its dir lazily, so construction does no I/O).
        let e = env(&[("XDG_RUNTIME_DIR", "/run/user/501")]);
        let _embedded = select_mux(Backend::Embedded, Path::new("/jail/home"), &e).unwrap();
        let _zmx = select_mux(Backend::Zmx, Path::new("/jail/home"), &e).unwrap();
    }

    #[test]
    fn embedded_env_snapshot_reuses_resolver() {
        // The snapshot env feeds resolve_qrmux_dir identically to the real Env.
        let e = env(&[("QD_HOME", "/relocated")]);
        let snap = env_snapshot(&e);
        let dir = crate::qrmux_dir::resolve_qrmux_dir(Path::new("/jail/home"), &snap).unwrap();
        assert_eq!(dir, Path::new("/relocated/mux"));
    }
}
