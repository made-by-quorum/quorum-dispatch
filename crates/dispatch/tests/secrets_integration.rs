//! A5 M2 integration coverage for the tiered secret store (`dispatch::secrets`),
//! exercising the PRODUCTION code paths against REAL effects:
//!
//! - **G-C1 shape** (file backend, real temp-HOME): a `set → get → unset`
//!   round-trip lands a real `~/.sb/config.toml` with `chmod 600` on disk (the
//!   permission posture asserted in ADR 0010 rider (a)).
//! - **G-C3 shape** (locked-keychain fallback, real subprocess): a PATH-shimmed
//!   fake `security` that emits the `User interaction is not allowed` signature
//!   (+ exit 36, mirroring the inbox live capture) drives the REAL `RealExec`
//!   seam → `set_secret` falls back to the file backend, writes the value, and
//!   chmods 600. Env-forced keychain fails loud with NO file write.
//!
//! These never invoke the real `security` binary (the shim shadows it on PATH)
//! and never touch the real Keychain. Test secrets are obviously-fake strings.

use dispatch::effects::Env;
use dispatch::exec::RealExec;
use dispatch::secrets::{
    delete_secret, get_secret, resolve_config_path, secret_backend_info, set_secret, Backend,
    SecretDeps,
};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

/// Serializes the two tests that mutate the process-global `PATH` (cargo may run
/// tests in a binary concurrently; `std::env::set_var` is process-wide).
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// A minimal env over a fixed map (we don't depend on the crate's MapEnv being
/// constructible from a slice in tests, so define a tiny local one).
struct TestEnv {
    vars: std::collections::HashMap<String, String>,
}
impl TestEnv {
    fn new(pairs: &[(&str, &str)]) -> Self {
        TestEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}
impl Env for TestEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn uid(&self) -> u32 {
        501
    }
}

/// Production-shaped real-fs closures + run a body over real `SecretDeps`. This
/// mirrors `bin/dispatch/config.rs::RealStore::with_deps` (the production binding) so
/// the integration test exercises the SAME fs behavior: mkdir -p, create 0600,
/// chmod-600-on-every-write.
fn with_real_deps<R>(
    env: &dyn Env,
    exec: &RealExec,
    platform: &str,
    keychain_available: bool,
    f: impl FnOnce(&SecretDeps) -> R,
) -> R {
    use std::fs;
    let read_file = |p: &str| fs::read_to_string(p).ok();
    let write_file = |p: &str, text: &str| {
        if let Some(parent) = Path::new(p).parent() {
            let _ = fs::create_dir_all(parent);
        }
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut fh) = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
        {
            use std::io::Write;
            let _ = fh.write_all(text.as_bytes());
        }
    };
    let chmod = |p: &str, mode: u32| {
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
    };
    let file_exists = |p: &str| Path::new(p).exists();
    let avail = || keychain_available;
    let notice = AtomicBool::new(false);
    let locked_diag = AtomicBool::new(false);
    let deps = SecretDeps {
        platform,
        env,
        exec,
        keychain_available: &avail,
        read_file: &read_file,
        write_file: &write_file,
        chmod: &chmod,
        file_exists: &file_exists,
        fallback_notice_emitted: &notice,
        locked_diag_emitted: &locked_diag,
    };
    f(&deps)
}

fn file_mode(path: &str) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// G-C1 shape: file backend, real temp-HOME, full round-trip + chmod 600 on disk.
#[test]
fn gc1_file_backend_roundtrip_real_fs_chmod_600() {
    let tmp = tempfile::tempdir().unwrap();
    let sb_home = tmp.path().join(".quorum").join("dispatch");
    let env = TestEnv::new(&[("SB_HOME", sb_home.to_str().unwrap())]);
    let exec = RealExec;

    with_real_deps(&env, &exec, "linux", false, |deps| {
        // set
        assert_eq!(
            set_secret("openrouter-key", "sk-or-v1-FAKE-roundtrip", deps),
            Ok(Backend::File)
        );
        let path = resolve_config_path(&env);
        assert!(Path::new(&path).exists(), "config.toml must exist");
        // chmod 600 landed on the REAL file.
        assert_eq!(file_mode(&path), 0o600, "config.toml must be 0600 on disk");
        // get round-trips
        assert_eq!(
            get_secret("openrouter-key", deps),
            Some("sk-or-v1-FAKE-roundtrip".to_string())
        );
        // backend_info: file backend, the key listed, value never present.
        let info = secret_backend_info(deps);
        assert_eq!(info.backend, Backend::File);
        // B4 item 1 affordance: keys carry the resolving tier (File here).
        assert_eq!(
            info.keys_set,
            vec![(
                "openrouter-key".to_string(),
                dispatch::secrets::Source::File
            )]
        );
        // unset removes it
        delete_secret("openrouter-key", deps);
        assert_eq!(get_secret("openrouter-key", deps), None);
        // file still 0600 after the unset rewrite
        assert_eq!(file_mode(&path), 0o600);
    });
}

/// Install a fake `security` shim into a temp PATH dir. `stderr_line` is echoed
/// to stderr and the script exits with `exit_code`. Returns the dir (keep it
/// alive) and the PATH string to use.
fn install_fake_security(stderr_line: &str, exit_code: i32) -> (tempfile::TempDir, String) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("security");
    let mut fh = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(&script)
        .unwrap();
    // bash 3.2-safe shim: emit the signature on stderr, exit with the code.
    write!(
        fh,
        "#!/bin/sh\n>&2 echo {}\nexit {}\n",
        shell_quote(stderr_line),
        exit_code
    )
    .unwrap();
    drop(fh);
    let existing = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{}", dir.path().display(), existing);
    (dir, path)
}

fn shell_quote(s: &str) -> String {
    // single-quote, escaping embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// G-C3 shape: SELECTED keychain locked → fallback to file via the REAL exec
/// seam driving a PATH-shimmed fake `security`. File write + chmod 600 land.
#[test]
fn gc3_locked_keychain_fallback_real_exec_path_shim() {
    let tmp = tempfile::tempdir().unwrap();
    let sb_home = tmp.path().join(".quorum").join("dispatch");
    let (_shim_dir, shim_path) = install_fake_security(
        "security: SecKeychainItemCreateFromContent (<default>): User interaction is not allowed.",
        36,
    );
    // PATH must include the shim so RealExec's `security` resolves to it.
    let env = TestEnv::new(&[("SB_HOME", sb_home.to_str().unwrap()), ("PATH", &shim_path)]);
    // RealExec inherits the PROCESS PATH, not the injected Env — so we must also
    // set the process PATH for the duration of this test. Save + restore under a
    // lock (set_var is process-global).
    let _guard = PATH_LOCK.lock().unwrap();
    let saved_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", &shim_path);
    let exec = RealExec;

    // platform "macos" + keychain_available true => keychain SELECTED (not forced).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_real_deps(&env, &exec, "macos", true, |deps| {
            // set hits the locked signature → falls back to file.
            assert_eq!(
                set_secret("openrouter-key", "sk-or-v1-FAKE-fallback", deps),
                Ok(Backend::File),
                "selected-keychain lock must fall back to file"
            );
            let path = resolve_config_path(&env);
            assert!(Path::new(&path).exists());
            assert_eq!(file_mode(&path), 0o600, "fallback file must be 0600");
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .contains("openrouter-key = \"sk-or-v1-FAKE-fallback\""));
        });
    }));

    // restore PATH before asserting (so a panic doesn't poison the env).
    match saved_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    result.unwrap();
}

/// G-C3 negative half: env-forced keychain under lock fails LOUD, no file write.
#[test]
fn gc3_env_forced_keychain_lock_fails_loud_no_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sb_home = tmp.path().join(".quorum").join("dispatch");
    let (_shim_dir, shim_path) =
        install_fake_security("security: User interaction is not allowed.", 36);
    let env = TestEnv::new(&[
        ("SB_HOME", sb_home.to_str().unwrap()),
        ("SB_SECRET_BACKEND", "keychain"),
        ("PATH", &shim_path),
    ]);
    let _guard = PATH_LOCK.lock().unwrap();
    let saved_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", &shim_path);
    let exec = RealExec;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_real_deps(&env, &exec, "macos", true, |deps| {
            let r = set_secret("openrouter-key", "sk-or-v1-FAKE-x", deps);
            assert!(r.is_err(), "env-forced keychain must fail loud under lock");
            assert!(r.unwrap_err().contains("forbids file fallback"));
            // NO file written.
            let path = resolve_config_path(&env);
            assert!(
                !Path::new(&path).exists(),
                "no fallback file may be written"
            );
        });
    }));

    match saved_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    result.unwrap();
}
