//! Getting `quorum-lane.ts` onto disk, and deciding where its socket lives.
//!
//! # Where the extension goes, and the consequence that shapes the file
//!
//! `~/.pi/agent/extensions/quorum-lane.ts` — pi's global auto-discovery
//! directory. That is a deliberate choice with one large consequence: pi loads
//! this extension into EVERY pi session on the box, not only the ones `qw`
//! launches. The extension is written to be inert without a socket path (see
//! its own module docs), and that inertness is what makes installing it here
//! acceptable rather than rude.
//!
//! The alternative — passing `--extension <path>` per launch — was not chosen,
//! but note it would not actually avoid the issue it appears to avoid: pi's
//! auto-discovery still runs alongside explicit `-e` paths unless
//! `--no-extensions` is also passed.
//!
//! # Why the source is baked into the binary
//!
//! [`include_str!`], the same mechanism `dispatch::extensions` uses for the pin
//! manifest. The extension and the client that speaks to it are two halves of
//! one protocol; shipping them in one artifact makes it impossible for a `qw`
//! to meet an extension from a different build. The install is idempotent and
//! content-addressed: if the file on disk already matches the baked bytes,
//! nothing is written and no jiti recompile is triggered.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::effects::Env;

/// The extension source, baked at build time.
///
/// A STRING, not a proof: `include_str!` cannot check that this TypeScript
/// loads, that pi's extension API still has the methods it calls, or that its
/// wire matches [`super::client`]. The live suite is what checks that.
pub const SOURCE: &str = include_str!("../../../../assets/pi-extension/quorum-lane.ts");

/// The filename installed into pi's extension directory. Stable — pi keys
/// discovery and `/reload` on the path.
pub const FILENAME: &str = "quorum-lane.ts";

#[derive(Debug)]
pub enum InstallError {
    NoHome,
    Io { path: String, detail: String },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::NoHome => write!(
                f,
                "cannot resolve HOME, so pi's extension directory cannot be located"
            ),
            InstallError::Io { path, detail } => {
                write!(f, "cannot install the pi extension at {path}: {detail}")
            }
        }
    }
}

/// What an [`install`] did. Distinguished because "already current" must not
/// touch the file: rewriting identical bytes changes the mtime, and jiti keys
/// its transpile cache on mtime — so a needless write buys a multi-second cold
/// compile on the very next launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// The file was absent and has been written.
    Fresh,
    /// The file held different bytes and has been overwritten.
    Updated,
    /// The file already matched. Nothing was written.
    AlreadyCurrent,
}

/// pi's global extension directory: `$HOME/.pi/agent/extensions`.
///
/// Resolved off the injected [`Env`] ONLY, never raw `std::env` — the same L9a
/// discipline as [`crate::provider::pi::sessions_root`], and the reason the
/// live tests can point a whole pi at a temp home.
pub fn extensions_dir(env: &dyn Env) -> Option<PathBuf> {
    let home = env.var("HOME").filter(|h| !h.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".pi")
            .join("agent")
            .join("extensions"),
    )
}

/// The installed path, whether or not it exists yet.
pub fn installed_path(env: &dyn Env) -> Option<PathBuf> {
    extensions_dir(env).map(|d| d.join(FILENAME))
}

/// Put the baked extension on disk, idempotently.
pub fn install(env: &dyn Env) -> Result<(PathBuf, Installed), InstallError> {
    let dir = extensions_dir(env).ok_or(InstallError::NoHome)?;
    let path = dir.join(FILENAME);

    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == SOURCE {
            return Ok((path, Installed::AlreadyCurrent));
        }
    }
    let fresh = !path.exists();

    std::fs::create_dir_all(&dir).map_err(|e| InstallError::Io {
        path: dir.display().to_string(),
        detail: e.to_string(),
    })?;

    // Write-and-rename: a pi starting up concurrently must never observe a
    // half-written extension. jiti would cache the truncated parse and the
    // session would come up without a control channel for reasons nothing
    // records.
    let tmp = dir.join(format!("{FILENAME}.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(SOURCE.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        InstallError::Io {
            path: path.display().to_string(),
            detail: e.to_string(),
        }
    })?;

    Ok((
        path,
        if fresh {
            Installed::Fresh
        } else {
            Installed::Updated
        },
    ))
}

/// The `sun_path` budget. macOS caps a unix socket path at 104 bytes
/// (`sizeof(sockaddr_un.sun_path)`); Linux allows 108. The smaller is used
/// everywhere so a path that works on one platform works on both.
///
/// This is not a tidiness limit. `bind(2)` is what fails, it fails INSIDE the
/// launched pi where the launcher cannot see it, and the visible symptom is a
/// session that came up perfectly and simply never opened a channel.
pub const SUN_PATH_MAX: usize = 104;

/// Where this session's control socket lives.
///
/// `$TMPDIR/quorum-pi/<16 hex>.sock`, and the shape matters in three ways:
///
///   - **Per session**, so two pi sessions never contend, and so the extension
///     may safely `rm` a stale path before binding — nothing else can be there.
///   - **Under a qw-owned directory** created `0700`, which is the actual access
///     control. The socket is a full remote-control surface for a live agent
///     session holding the user's credentials.
///   - **Short**, which is why the filename is a HASH of the session id rather
///     than the id itself. See [`socket_path`].
pub fn socket_dir(env: &dyn Env) -> PathBuf {
    let tmp = env
        .var("TMPDIR")
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(tmp.trim_end_matches('/')).join("quorum-pi")
}

/// This session's socket path: the session id HASHED to 16 hex characters.
///
/// # Why a hash and not the id
///
/// A session id is a 36-character UUID, and `<uuid>.sock` is 41 bytes before
/// any directory. Measured against a real jail — a `$TMPDIR` of
/// `/tmp/qd-piext/qdrg-runs/piext-<nanos>/tmp` — the full path came to **105
/// bytes against a 104 cap**, and the failure mode was exactly the one
/// [`SUN_PATH_MAX`] describes: the pane was healthy, the row was correct, and
/// the socket silently never appeared. macOS's own `$TMPDIR`
/// (`/var/folders/xx/…/T/`) is ~49 bytes, which leaves the UUID form with
/// almost nothing to spare.
///
/// Sixteen hex characters of SHA-256 is 21 bytes with the suffix — half the
/// UUID form — and collision is not a practical concern at 2^64 over the
/// handful of live sessions one machine hosts. Determinism is what matters: the
/// same session must derive the same path every time, because this is the
/// fallback [`super::create::socket_for`] uses when a row records no endpoint.
pub fn socket_path(env: &dyn Env, session_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(session_id.as_bytes());
    let short: String = digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    socket_dir(env).join(format!("{short}.sock"))
}

/// Would this path overrun the kernel's `sun_path`?
///
/// Checked by the launch BEFORE anything is claimed or spawned, so an
/// environment that cannot host a control channel is refused with a sentence
/// naming the path and the limit — rather than producing a session whose
/// channel never binds for reasons nothing records.
pub fn socket_path_too_long(path: &Path) -> Option<String> {
    let len = path.as_os_str().as_encoded_bytes().len();
    (len >= SUN_PATH_MAX).then(|| {
        format!(
            "the control socket path is {len} bytes, over the {SUN_PATH_MAX}-byte \
             kernel limit for a unix socket: {}. Set a shorter TMPDIR.",
            path.display()
        )
    })
}

/// Create the socket directory `0700`. Called before a launch that will bind
/// inside it, because node's `mkdir` would create it `0755`.
pub fn ensure_socket_dir(env: &dyn Env) -> Result<PathBuf, InstallError> {
    let dir = socket_dir(env);
    std::fs::create_dir_all(&dir).map_err(|e| InstallError::Io {
        path: dir.display().to_string(),
        detail: e.to_string(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// Remove a session's socket. Best-effort: the extension unlinks it on a clean
/// shutdown, so this is for the kill path, where the process was not asked
/// politely.
pub fn remove_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Env;
    use std::collections::HashMap;

    /// A test `Env` backed by a fixed map (no raw `std::env`, L9a) — the same
    /// shape every other test module in this crate defines for itself.
    struct MapEnv(HashMap<String, String>);
    impl Env for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn uid(&self) -> u32 {
            0
        }
    }
    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
    fn env_home(home: &Path) -> MapEnv {
        env(&[("HOME", home.to_string_lossy().as_ref())])
    }

    #[test]
    fn install_writes_then_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let env = env_home(home.path());

        let (path, first) = install(&env).unwrap();
        assert_eq!(first, Installed::Fresh);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);

        let (_, second) = install(&env).unwrap();
        assert_eq!(
            second,
            Installed::AlreadyCurrent,
            "a matching file must not be rewritten — the mtime is jiti's cache key"
        );
    }

    #[test]
    fn install_replaces_a_stale_file() {
        let home = tempfile::tempdir().unwrap();
        let env = env_home(home.path());
        let (path, _) = install(&env).unwrap();
        std::fs::write(&path, "// an older build\n").unwrap();

        let (_, again) = install(&env).unwrap();
        assert_eq!(again, Installed::Updated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
    }

    #[test]
    fn no_home_is_named_not_guessed() {
        let env = env(&[]);
        assert!(matches!(install(&env), Err(InstallError::NoHome)));
    }

    /// The baked source must actually be the extension, not an empty or
    /// truncated include. Cheap, and it fails loudly if the asset moves.
    #[test]
    fn baked_source_looks_like_the_extension() {
        assert!(SOURCE.contains("quorum-sock"), "must register the flag");
        assert!(
            SOURCE.contains("sendUserMessage"),
            "deliver rides pi.sendUserMessage"
        );
        assert!(
            SOURCE.contains("agent_settled"),
            "idle must come from agent_settled, not agent_end"
        );
    }

    /// The path-length trap, pinned against the REAL environment that tripped
    /// it: the integration jail's `$TMPDIR`. With the session id spelled out in
    /// full this came to 105 bytes against a 104-byte cap, and `bind(2)` failed
    /// inside the launched process where nothing could report it.
    #[test]
    fn socket_path_fits_in_sun_path_even_in_the_deepest_tmpdir_we_use() {
        for tmpdir in [
            // The integration jail — the one that actually overran.
            "/tmp/qd-piext/qdrg-runs/piext-1786998026685721000/tmp",
            // macOS's own per-user temp dir.
            "/var/folders/lp/9k3ydbhd425cttywd3twg2f80000gn/T/",
            "/tmp",
        ] {
            let e = env(&[("TMPDIR", tmpdir)]);
            let p = socket_path(&e, "d0c295a4-9f54-4f35-95cd-05cd58e9c34d");
            let len = p.as_os_str().as_encoded_bytes().len();
            assert!(
                len < SUN_PATH_MAX,
                "socket path is {len} bytes under TMPDIR={tmpdir} — bind(2) would \
                 fail invisibly: {}",
                p.display()
            );
            assert_eq!(socket_path_too_long(&p), None);
        }
    }

    /// Determinism: the derivation is the FALLBACK for a row with no recorded
    /// endpoint, so the same session must always derive the same path.
    #[test]
    fn socket_path_is_deterministic_for_a_session() {
        let e = env(&[("TMPDIR", "/tmp")]);
        assert_eq!(socket_path(&e, "abc"), socket_path(&e, "abc"));
    }

    /// An environment that cannot host a channel is NAMED, not silently broken.
    #[test]
    fn an_overlong_path_is_refused_with_the_length_and_the_fix() {
        let deep = "/tmp/".to_string() + &"x".repeat(120);
        let e = env(&[("TMPDIR", deep.as_str())]);
        let p = socket_path(&e, "abc");
        let why = socket_path_too_long(&p).expect("must refuse");
        assert!(why.contains("over the 104-byte"), "{why}");
        assert!(why.contains("TMPDIR"), "the message must say what to change: {why}");
    }

    #[test]
    fn socket_path_is_per_session() {
        let env = env(&[("TMPDIR", "/tmp")]);
        assert_ne!(socket_path(&env, "a"), socket_path(&env, "b"));
    }
}
