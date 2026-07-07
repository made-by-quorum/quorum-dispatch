//! `qd config` — hand-parsed (spec §2: config bypasses commander with
//! allowUnknownOption + allowExcessArguments + helpOption(false), commands/config.ts:222-
//! 224). Port of `runConfigLogic` (commands/config.ts:74-164) + `helpText` (commands/config.ts:54-
//! 72) as a PURE function over an injected [`SecretStore`].
//!
//! The arg-handling / usage / help / exit-codes are REAL (unit-tested below,
//! ported from config.test.ts). As of A5 (M2) the secret-store backend is REAL:
//! [`RealStore`] wires the tiered store in [`dispatch::secrets`] (macOS Keychain when
//! available, else a chmod-600 `~/.quorum/dispatch/config.toml`), including the ADR 0010
//! locked-keychain auto-fallback. The hidden prompt is wired in [`dispatch`]
//! (termios echo-off on `/dev/tty`); a non-TTY one-arg `set` fails loud (A5
//! §3.3, named divergence — TS attempts the prompt and breaks under zmx).

use dispatch::secrets::{
    env_var_for_key, is_known_key as secrets_is_known_key, is_plain_file_key,
    known_key_names as secrets_known_key_names, mask_secret, validate_key_value,
};

fn is_known_key(k: &str) -> bool {
    secrets_is_known_key(k)
}

fn known_key_names() -> Vec<&'static str> {
    secrets_known_key_names()
}

/// `(selected-backend label, config file path, [(key name, resolving tier)])`
/// — the `config path` payload. Each set key carries the tier it resolves from
/// (B4 item 1 affordance). Aliased to keep the trait signature readable.
pub type BackendInfoTuple = (String, String, Vec<(String, String)>);

/// Injected secret store (the A5 backend seam). `get`/`set`/`unset` return
/// `Result<_, String>` so the production stub can fail honestly; `backend_info`
/// reports the active backend + which keys are set.
pub trait SecretStore {
    /// Read a stored secret value with the TIER it resolved from (B4 item 1
    /// affordance): `Some((value, tier))` where `tier` is "env" / "keychain" /
    /// "file"; `None` = unset. Err = backend failure.
    fn get(&self, key: &str) -> Result<Option<(String, String)>, String>;
    /// Store a secret value; returns the backend label that ACTUALLY stored it
    /// (A5 §3.2: under ADR 0010 fallback this is `"file"` even when the selected
    /// backend is keychain — the set-line reports the truthful storing backend).
    /// Err = backend failure.
    fn set(&mut self, key: &str, value: &str) -> Result<String, String>;
    /// Delete a stored secret. Err = backend failure.
    fn unset(&mut self, key: &str) -> Result<(), String>;
    /// See [`BackendInfoTuple`]: selected backend, config path, per-key tiers.
    fn backend_info(&self) -> Result<BackendInfoTuple, String>;
}

/// The result of `run_config_logic` (config.ts ConfigResult).
#[derive(Debug, PartialEq)]
pub struct ConfigResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn ok(stdout: String) -> ConfigResult {
    ConfigResult {
        exit_code: 0,
        stdout,
        stderr: String::new(),
    }
}

fn fail(message: &str, code: i32) -> ConfigResult {
    ConfigResult {
        exit_code: code,
        stdout: String::new(),
        stderr: format!("{message}\n"),
    }
}

fn unknown_key_error(key: &str) -> ConfigResult {
    fail(
        &format!(
            "qd config: unknown key '{key}'. Known keys: {}.",
            known_key_names().join(", ")
        ),
        2,
    )
}

/// `helpText()` (commands/config.ts:54-72).
pub fn help_text() -> String {
    let known: String = known_key_names()
        .iter()
        .map(|k| match env_var_for_key(k) {
            Some(env) => format!("  {k}  (overridden by env {env})"),
            // Plain file-tier keys (punch item 7): no env override; teach the
            // legal values for render-default inline.
            None if *k == "render-default" => {
                format!("  {k}  (inline | alt-screen; plain config, file tier)")
            }
            None => format!("  {k}  (plain config, file tier)"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Usage: qd config <set|get|unset|path> [key] [value] [--reveal]\n\nManage stored secrets. Secrets live in the macOS Keychain when available, else\na chmod-600 ~/.quorum/dispatch/config.toml. An env var ALWAYS overrides the stored value.\n\nCommands:\n  qd config set <key>           Prompt (hidden) for the value and store it\n  qd config set <key> <value>   Store non-interactively (value may land in shell history)\n  qd config get <key>           Print the stored value MASKED (--reveal for full)\n  qd config unset <key>         Delete the stored value\n  qd config path                Show active backend + config path + which keys are set\n\nKnown keys:\n{known}"
    )
}

/// Port of `runConfigLogic` (commands/config.ts:74-164). PURE over the injected store.
///
/// `prompt_value` stands in for the hidden-prompt dep (config.ts promptHidden):
/// when `set <key>` is given with NO value, this is the value that would be
/// entered (None = empty / no prompt available → the production binding wires a
/// real hidden prompt; tests inject a value).
pub fn run_config_logic(
    argv: &[String],
    store: &mut dyn SecretStore,
    prompt_value: Option<&str>,
) -> ConfigResult {
    // Bare / --help / -h → help to stdout, exit 0 (commands/config.ts:75-77).
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        return ok(format!("{}\n", help_text()));
    }

    let sub = argv[0].as_str();
    let rest = &argv[1..];

    match sub {
        "set" => run_set(rest, store, prompt_value),
        "get" => run_get(rest, store),
        "unset" => run_unset(rest, store),
        "path" => run_path(rest, store),
        // Unknown subcommand → exit 2 (commands/config.ts:92-95).
        _ => fail(
            &format!("qd config: unknown subcommand '{sub}'. Use set, get, unset, or path."),
            2,
        ),
    }
}

fn run_set(
    rest: &[String],
    store: &mut dyn SecretStore,
    prompt_value: Option<&str>,
) -> ConfigResult {
    let Some(key) = rest.first() else {
        return fail(
            "qd config set: a key is required (e.g. `qd config set openrouter-key`).",
            2,
        );
    };
    if !is_known_key(key) {
        return unknown_key_error(key);
    }

    let mut warning = String::new();
    let value: String = if rest.len() >= 2 {
        // Value on the CLI: works, but warn about shell history (commands/config.ts:106-111).
        // Plain (non-secret) keys skip the warning — there is no secret to leak.
        if !is_plain_file_key(key) {
            warning = format!(
                "qd config: warning -- passing a secret on the command line may leave it in your shell history. Prefer `qd config set {key}` (hidden prompt).\n"
            );
        }
        rest[1..].join(" ")
    } else {
        let v = prompt_value.unwrap_or("").trim().to_string();
        if v.is_empty() {
            return fail("qd config set: empty value; nothing stored.", 1);
        }
        v
    };

    // Punch item 7 (errors-that-teach): value-validated keys (render-default)
    // reject anything but their legal values AT SET TIME, exit 2, nothing stored.
    if let Some(msg) = validate_key_value(key, &value) {
        return fail(&msg, 2);
    }

    let backend = match store.set(key, &value) {
        Ok(b) => b,
        Err(e) => return fail(&e, 1),
    };
    ConfigResult {
        exit_code: 0,
        stdout: format!("{warning}Stored {key} (backend: {backend}).\n"),
        stderr: String::new(),
    }
}

fn run_get(rest: &[String], store: &mut dyn SecretStore) -> ConfigResult {
    // Parse [key] + optional --reveal in any order (commands/config.ts:128-135).
    let mut key: Option<&str> = None;
    let mut reveal = false;
    for a in rest {
        if a == "--reveal" {
            reveal = true;
        } else if a.starts_with('-') {
            return fail(&format!("qd config get: unknown option '{a}'."), 2);
        } else if key.is_none() {
            key = Some(a);
        } else {
            return fail(&format!("qd config get: unexpected argument '{a}'."), 2);
        }
    }
    let Some(key) = key else {
        return fail("qd config get: a key is required.", 2);
    };
    if !is_known_key(key) {
        return unknown_key_error(key);
    }

    match store.get(key) {
        Ok(None) => ok(format!("{key}: not set.\n")),
        Ok(Some((value, tier))) => {
            // Plain (non-secret) keys print unmasked — masking "inline" would
            // be noise, not protection (punch item 7).
            let shown = if reveal || is_plain_file_key(key) {
                value
            } else {
                mask_secret(&value)
            };
            // B4 item 1 affordance: surface WHICH TIER resolved the value
            // (env / keychain / file). The tier label is not a secret; the
            // VALUE stays masked unless --reveal. Converts future tier drift
            // from archaeology to a one-line diagnosis.
            ok(format!("{key}: {shown}  (tier: {tier})\n"))
        }
        Err(e) => fail(&e, 1),
    }
}

fn run_unset(rest: &[String], store: &mut dyn SecretStore) -> ConfigResult {
    let Some(key) = rest.first() else {
        return fail("qd config unset: a key is required.", 2);
    };
    if !is_known_key(key) {
        return unknown_key_error(key);
    }
    if rest.len() > 1 {
        return fail(
            &format!("qd config unset: unexpected argument '{}'.", rest[1]),
            2,
        );
    }

    let had = matches!(store.get(key), Ok(Some(_)));
    // NOTE: `unset` removes from whichever tier holds the value (delete_secret's
    // existing cross-tier clear is UNCHANGED — B4 added read symmetry only).
    if let Err(e) = store.unset(key) {
        return fail(&e, 1);
    }
    ok(if had {
        format!("Unset {key}.\n")
    } else {
        format!("{key} was not set; nothing to unset.\n")
    })
}

fn run_path(rest: &[String], store: &mut dyn SecretStore) -> ConfigResult {
    if !rest.is_empty() {
        return fail(
            &format!("qd config path: unexpected argument '{}'.", rest[0]),
            2,
        );
    }
    match store.backend_info() {
        Ok((backend, file_path, keys_set)) => {
            let file_note = if backend == "keychain" {
                " (file backend; secrets may also be stored here)"
            } else {
                ""
            };
            // B4 item 1 affordance: each set key shows the tier it resolves
            // from — a file-stranded key now reads `openrouter-key (file)`
            // instead of vanishing under the old "(none)" while config.toml
            // held it.
            let keys = if keys_set.is_empty() {
                "(none)".to_string()
            } else {
                keys_set
                    .iter()
                    .map(|(k, tier)| format!("{k} ({tier})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            ok(format!(
                "Backend:     {backend}\nConfig file: {file_path}{file_note}\nKeys set:    {keys}\n"
            ))
        }
        Err(e) => fail(&e, 1),
    }
}

// --- production binding (A5 M2: real tiered store) ---

/// The REAL secret store (A5 M2): binds the [`dispatch::secrets`] tiered backend
/// (macOS Keychain when available, else a chmod-600 `~/.quorum/dispatch/config.toml`),
/// including the ADR 0010 locked-keychain auto-fallback. Owns the production
/// seams ([`RealEnv`]/[`RealExec`]) and the per-process fallback-notice flag,
/// and builds a [`dispatch::secrets::SecretDeps`] over real-fs closures for each op.
struct RealStore {
    env: dispatch::effects::RealEnv,
    exec: dispatch::exec::RealExec,
    /// One-per-process guard for the ADR 0010 fallback notice.
    notice: std::sync::atomic::AtomicBool,
    /// One-per-process guard for the env-forced-locked GET diagnostic (orc-2
    /// ruling relay-1780639217973-4). Separate flag from `notice` so the two
    /// divergence lines never share a guard.
    locked_diag: std::sync::atomic::AtomicBool,
}

impl RealStore {
    fn new() -> Self {
        RealStore {
            env: dispatch::effects::RealEnv,
            exec: dispatch::exec::RealExec,
            notice: std::sync::atomic::AtomicBool::new(false),
            locked_diag: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Build a [`dispatch::secrets::SecretDeps`] over the real seams + real-fs closures
    /// and run `f`. The closures live on this call's stack (they cannot outlive a
    /// returned `SecretDeps`), so every op runs inside the callback.
    fn with_deps<R>(&self, f: impl FnOnce(&dispatch::secrets::SecretDeps) -> R) -> R {
        use std::fs;
        // Real-fs closures (the production analogue of TS `defaultSecretDeps`,
        // 0d0fa9e:src/secrets.ts:332-360): read = None on any error; write =
        // mkdir -p parent + create 0600 up front (chmod afterward still
        // enforces); chmod = best-effort; exists = path test.
        let read_file = |p: &str| fs::read_to_string(p).ok();
        let write_file = |p: &str, text: &str| {
            if let Some(parent) = std::path::Path::new(p).parent() {
                let _ = fs::create_dir_all(parent);
            }
            // Create with restrictive perms up front; the chmod closure enforces
            // 0600 on every write regardless.
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
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
        };
        let file_exists = |p: &str| std::path::Path::new(p).exists();
        // Probe keychain availability lazily; cache via the closure's own call.
        let keychain_available = || dispatch::secrets::real_keychain_available(&self.exec);
        let deps = dispatch::secrets::SecretDeps {
            platform: std::env::consts::OS,
            env: &self.env,
            exec: &self.exec,
            keychain_available: &keychain_available,
            read_file: &read_file,
            write_file: &write_file,
            chmod: &chmod,
            file_exists: &file_exists,
            fallback_notice_emitted: &self.notice,
            locked_diag_emitted: &self.locked_diag,
        };
        f(&deps)
    }
}

impl SecretStore for RealStore {
    fn get(&self, key: &str) -> Result<Option<(String, String)>, String> {
        // B4 item 1: resolve through the shared precedence so `config get`
        // reports the tier AND honors the clean-miss file fallthrough.
        Ok(self.with_deps(|deps| {
            let r = dispatch::secrets::resolve_config_tier(key, deps);
            match (r.value, r.source) {
                (Some(v), Some(s)) => Some((v, s.label().to_string())),
                _ => None,
            }
        }))
    }
    fn set(&mut self, key: &str, value: &str) -> Result<String, String> {
        self.with_deps(|deps| dispatch::secrets::set_secret(key, value, deps))
            .map(|backend| backend.label().to_string())
    }
    fn unset(&mut self, key: &str) -> Result<(), String> {
        self.with_deps(|deps| dispatch::secrets::delete_secret(key, deps));
        Ok(())
    }
    fn backend_info(&self) -> Result<BackendInfoTuple, String> {
        let info = self.with_deps(dispatch::secrets::secret_backend_info);
        Ok((
            info.backend.label().to_string(),
            info.file_path,
            info.keys_set
                .into_iter()
                .map(|(k, s)| (k, s.label().to_string()))
                .collect(),
        ))
    }
}

/// Production entry: hand the argv tail to `run_config_logic` with the REAL
/// tiered store. The hidden prompt (config.ts realPromptHidden) is wired here:
///
/// - One-arg `set <key>` (no value) on a TTY → hidden prompt (termios echo-off
///   on `/dev/tty`, restored on drop).
/// - One-arg `set <key>` (no value) with stdin NOT a TTY → loud exit 1 (A5
///   §3.3, named divergence — TS attempts the prompt and breaks under zmx).
/// - Two-arg / non-`set` forms never prompt.
pub fn dispatch(argv_tail: &[String]) -> i32 {
    let mut store = RealStore::new();

    // Is this the interactive one-arg `set <key>` form that needs a prompt?
    // (`set` + exactly one non-flag arg that is a known SECRET key — mirror the
    // pure logic's gate so we only prompt when it actually would. Plain keys
    // (render-default) never hidden-prompt: their values are not secrets, and a
    // hidden prompt for "inline" would be baffling — the no-value form falls
    // through to the pure logic's "empty value; nothing stored" error.)
    let needs_prompt = argv_tail.first().map(String::as_str) == Some("set")
        && argv_tail.len() == 2
        && is_known_key(&argv_tail[1])
        && !is_plain_file_key(&argv_tail[1]);

    let prompted: Option<String> = if needs_prompt {
        if !stdin_is_tty() {
            // A5 §3.3: non-TTY one-arg set fails LOUD (exit 1) with an actionable
            // message — never a broken/hung prompt.
            let key = &argv_tail[1];
            eprintln!(
                "qd config set: stdin is not a TTY; pass the value as an argument or use QD_SECRET_BACKEND=file qd config set {key} <value>."
            );
            return 1;
        }
        match prompt_hidden(&format!("Value for {}: ", argv_tail[1])) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("qd config set: {e}");
                return 1;
            }
        }
    } else {
        None
    };

    let result = run_config_logic(argv_tail, &mut store, prompted.as_deref());
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    result.exit_code
}

/// Is stdin a TTY? (A5 §3.3 non-TTY gate.)
fn stdin_is_tty() -> bool {
    // SAFETY: isatty on a valid fd is always safe.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Read a line from the user WITHOUT echoing it (hidden prompt). Port of
/// `realPromptHidden` (0d0fa9e:src/commands/config.ts:166-209), reshaped to a
/// blocking termios read on `/dev/tty`:
///
/// - Open `/dev/tty` directly (NOT stdin — the value must be read from the
///   controlling terminal even if stdin is redirected; matches the TS intent of
///   reading the interactive terminal).
/// - Disable ECHO (+ canonical mode) via termios; restore the original attrs on
///   drop (RAII guard) so a panic/early-return never leaves the terminal with
///   echo off.
/// - The label is written to stderr (TS writes to stderr); a trailing newline is
///   printed after Enter. The returned value is NOT trimmed here — `run_set`
///   trims + applies the empty-value check (TS parity).
fn prompt_hidden(label: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::io::AsRawFd;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("cannot open /dev/tty for hidden prompt: {e}"))?;
    let fd = tty.as_raw_fd();

    // RAII guard: capture the original termios, restore on drop.
    let guard = TermiosGuard::echo_off(fd)?;

    // Write the label to stderr (matches TS `process.stderr.write(label)`).
    eprint!("{label}");
    let _ = std::io::stderr().flush();

    let mut reader = BufReader::new(&tty);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("hidden prompt read failed: {e}"))?;

    // Echo a newline (the user's Enter was swallowed by no-echo).
    eprintln!();
    drop(guard); // restore echo before returning

    // Strip the trailing newline(s) the read captured; keep the rest verbatim.
    let value = line.trim_end_matches(['\n', '\r']).to_string();
    Ok(value)
}

/// RAII termios guard: turns ECHO (and canonical mode) off on construction,
/// restores the original attrs on drop. Keeps the terminal usable even if the
/// caller panics or returns early.
struct TermiosGuard {
    fd: i32,
    original: libc::termios,
}

impl TermiosGuard {
    fn echo_off(fd: i32) -> Result<Self, String> {
        // SAFETY: termios is POD; we initialize it via tcgetattr before reading.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err("tcgetattr failed on /dev/tty".to_string());
        }
        let mut raw = original;
        // Clear ECHO + ICANON-stays-on for line input but no echo: TS reads char
        // by char; a blocking line read needs canonical mode ON (so Enter
        // submits) with ECHO OFF.
        raw.c_lflag &= !(libc::ECHO);
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err("tcsetattr failed on /dev/tty".to_string());
        }
        Ok(TermiosGuard { fd, original })
    }
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        // Best-effort restore; nothing actionable if it fails.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake file-backed store (the Rust analogue of config.test.ts makeFileFs):
    /// arg-handling tests run with NO keychain, NO real fs.
    #[derive(Default)]
    struct FakeStore {
        files: HashMap<String, String>,
        backend: String,
        path: String,
    }

    impl FakeStore {
        fn new() -> Self {
            FakeStore {
                files: HashMap::new(),
                backend: "file".to_string(),
                path: "/quorum/qd/config.toml".to_string(),
            }
        }
    }

    impl SecretStore for FakeStore {
        fn get(&self, key: &str) -> Result<Option<(String, String)>, String> {
            // The fake stores file-tier values; report tier "file".
            Ok(self
                .files
                .get(key)
                .cloned()
                .map(|v| (v, "file".to_string())))
        }
        fn set(&mut self, key: &str, value: &str) -> Result<String, String> {
            self.files.insert(key.to_string(), value.to_string());
            Ok(self.backend.clone())
        }
        fn unset(&mut self, key: &str) -> Result<(), String> {
            self.files.remove(key);
            Ok(())
        }
        fn backend_info(&self) -> Result<BackendInfoTuple, String> {
            let mut keys: Vec<(String, String)> = self
                .files
                .keys()
                .cloned()
                .map(|k| (k, "file".to_string()))
                .collect();
            keys.sort();
            Ok((self.backend.clone(), self.path.clone(), keys))
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // --- help / dispatch (config.test.ts) ---

    #[test]
    fn no_args_prints_help_exit_0() {
        let mut s = FakeStore::new();
        let r = run_config_logic(&[], &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("Usage: qd config"));
    }

    #[test]
    fn help_flag_prints_help() {
        let mut s = FakeStore::new();
        assert!(run_config_logic(&argv(&["--help"]), &mut s, None)
            .stdout
            .contains("Usage: qd config"));
    }

    #[test]
    fn help_text_lists_known_keys() {
        assert!(help_text().contains("openrouter-key"));
        assert!(help_text().contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn unknown_subcommand_exit_2() {
        let mut s = FakeStore::new();
        let r = run_config_logic(&argv(&["frobnicate"]), &mut s, None);
        assert_eq!(r.exit_code, 2);
        assert!(r.stderr.contains("unknown subcommand"));
    }

    // --- set ---

    #[test]
    fn set_prompts_when_no_value_stores_it() {
        let mut s = FakeStore::new();
        let r = run_config_logic(
            &argv(&["set", "openrouter-key"]),
            &mut s,
            Some("sk-or-prompted9999"),
        );
        assert_eq!(r.exit_code, 0);
        assert_eq!(
            s.files.get("openrouter-key").map(String::as_str),
            Some("sk-or-prompted9999")
        );
        assert!(r.stdout.contains("Stored openrouter-key"));
        assert!(!r.stdout.contains("sk-or-prompted9999")); // never echo value
    }

    #[test]
    fn set_value_on_cli_warns_about_shell_history() {
        let mut s = FakeStore::new();
        let r = run_config_logic(
            &argv(&["set", "openrouter-key", "sk-or-cli8888"]),
            &mut s,
            None,
        );
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("warning"));
        assert!(r.stdout.contains("shell history"));
        assert_eq!(
            s.files.get("openrouter-key").map(String::as_str),
            Some("sk-or-cli8888")
        );
    }

    /// A5 §3.2: the `set` success line reports the backend that ACTUALLY stored
    /// the value (what `store.set` returns), NOT the selected backend from
    /// `backend_info`. Under ADR 0010 fallback the store reports `file` even
    /// though the selection is `keychain`; this test pins that the pure logic
    /// uses the storing backend. The `FallbackStore` selection is `keychain` but
    /// its `set` returns `file` (the fallback storing backend).
    #[test]
    fn set_success_line_reports_actual_storing_backend() {
        struct FallbackStore;
        impl SecretStore for FallbackStore {
            fn get(&self, _k: &str) -> Result<Option<(String, String)>, String> {
                Ok(None)
            }
            fn set(&mut self, _k: &str, _v: &str) -> Result<String, String> {
                // Fallback fired: stored in FILE even though selection=keychain.
                Ok("file".to_string())
            }
            fn unset(&mut self, _k: &str) -> Result<(), String> {
                Ok(())
            }
            fn backend_info(&self) -> Result<BackendInfoTuple, String> {
                // Selection stays keychain (the A5 §3.2 split).
                Ok(("keychain".to_string(), "/x/config.toml".to_string(), vec![]))
            }
        }
        let mut s = FallbackStore;
        let r = run_config_logic(
            &argv(&["set", "openrouter-key", "sk-or-FAKE-v"]),
            &mut s,
            None,
        );
        assert_eq!(r.exit_code, 0);
        // Must report the STORING backend (file), not the selection (keychain).
        assert!(r.stdout.contains("(backend: file)"));
        assert!(!r.stdout.contains("(backend: keychain)"));
    }

    #[test]
    fn set_empty_prompt_value_errors_nothing_stored() {
        let mut s = FakeStore::new();
        let r = run_config_logic(&argv(&["set", "openrouter-key"]), &mut s, Some("   "));
        assert_eq!(r.exit_code, 1);
        assert!(!s.files.contains_key("openrouter-key"));
    }

    #[test]
    fn set_unknown_key_exit_2_no_prompt() {
        let mut s = FakeStore::new();
        let r = run_config_logic(&argv(&["set", "bogus-key"]), &mut s, None);
        assert_eq!(r.exit_code, 2);
        assert!(r.stderr.contains("unknown key"));
        assert!(r.stderr.contains("openrouter-key"));
    }

    #[test]
    fn set_missing_key_exit_2() {
        let mut s = FakeStore::new();
        assert_eq!(run_config_logic(&argv(&["set"]), &mut s, None).exit_code, 2);
    }

    // --- render-default (punch item 7: plain file-tier key) ---

    /// set/get round-trip: legal values store and read back UNMASKED, with NO
    /// shell-history warning (not a secret); unset removes.
    #[test]
    fn render_default_set_get_unset_round_trip() {
        let mut s = FakeStore::new();
        let r = run_config_logic(
            &argv(&["set", "render-default", "alt-screen"]),
            &mut s,
            None,
        );
        assert_eq!(r.exit_code, 0);
        assert!(
            !r.stdout.contains("warning"),
            "plain key must not warn about shell history: {}",
            r.stdout
        );
        let r = run_config_logic(&argv(&["get", "render-default"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        // Unmasked value + the B4 tier affordance (file-tier plain key).
        assert_eq!(r.stdout, "render-default: alt-screen  (tier: file)\n");
        let r = run_config_logic(&argv(&["unset", "render-default"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(
            run_config_logic(&argv(&["get", "render-default"]), &mut s, None)
                .stdout
                .contains("not set")
        );
    }

    /// Invalid value → teaching error (exit 2, names both legal values),
    /// nothing stored.
    #[test]
    fn render_default_invalid_value_teaching_error() {
        let mut s = FakeStore::new();
        let r = run_config_logic(
            &argv(&["set", "render-default", "fullscreen"]),
            &mut s,
            None,
        );
        assert_eq!(r.exit_code, 2);
        assert!(r.stderr.contains("invalid value 'fullscreen'"));
        assert!(r.stderr.contains("'inline'"));
        assert!(r.stderr.contains("'alt-screen'"));
        assert!(!s.files.contains_key("render-default"), "nothing stored");
    }

    #[test]
    fn help_text_lists_render_default_with_values() {
        assert!(help_text().contains("render-default"));
        assert!(help_text().contains("inline | alt-screen"));
    }

    // --- get ---

    #[test]
    fn get_masked_by_default() {
        let mut s = FakeStore::new();
        s.files
            .insert("openrouter-key".into(), "sk-or-v1-abcd1234".into());
        let r = run_config_logic(&argv(&["get", "openrouter-key"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("••••1234"));
        assert!(!r.stdout.contains("sk-or-v1-abcd1234"));
        // B4 affordance: the resolving tier is surfaced (value still masked).
        assert!(r.stdout.contains("(tier: file)"));
    }

    #[test]
    fn get_reveal_prints_full() {
        let mut s = FakeStore::new();
        s.files
            .insert("openrouter-key".into(), "sk-or-v1-abcd1234".into());
        assert!(
            run_config_logic(&argv(&["get", "openrouter-key", "--reveal"]), &mut s, None)
                .stdout
                .contains("sk-or-v1-abcd1234")
        );
    }

    #[test]
    fn get_reveal_before_key_order_agnostic() {
        let mut s = FakeStore::new();
        s.files
            .insert("openrouter-key".into(), "sk-or-v1-abcd1234".into());
        assert!(
            run_config_logic(&argv(&["get", "--reveal", "openrouter-key"]), &mut s, None)
                .stdout
                .contains("sk-or-v1-abcd1234")
        );
    }

    #[test]
    fn get_not_set_friendly_message() {
        let mut s = FakeStore::new();
        assert!(
            run_config_logic(&argv(&["get", "openrouter-key"]), &mut s, None)
                .stdout
                .contains("not set")
        );
    }

    #[test]
    fn get_unknown_key_exit_2() {
        let mut s = FakeStore::new();
        assert_eq!(
            run_config_logic(&argv(&["get", "nope"]), &mut s, None).exit_code,
            2
        );
    }

    #[test]
    fn get_unknown_option_exit_2() {
        let mut s = FakeStore::new();
        assert_eq!(
            run_config_logic(&argv(&["get", "openrouter-key", "--bogus"]), &mut s, None).exit_code,
            2
        );
    }

    /// G-C6 (A5 carry 3 / N1 resolution): `config get` with NO key → exit 2 and
    /// the EXACT byte-stderr the pin emits (`runGet` no-key,
    /// `0d0fa9e:src/commands/config.ts:135`). A3 matrix N1 observed exit 0; that
    /// observation is STALE at the pin — the pin is exit 2. This row pins the
    /// byte-for-byte stderr so the regression can never reappear.
    #[test]
    fn get_no_key_exit_2_byte_stderr_gc6() {
        let mut s = FakeStore::new();
        let r = run_config_logic(&argv(&["get"]), &mut s, None);
        assert_eq!(r.exit_code, 2);
        assert_eq!(r.stderr, "qd config get: a key is required.\n");
        assert!(r.stdout.is_empty());
    }

    // --- unset ---

    #[test]
    fn unset_removes_set_key() {
        let mut s = FakeStore::new();
        s.files.insert("openrouter-key".into(), "sk".into());
        let r = run_config_logic(&argv(&["unset", "openrouter-key"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("Unset openrouter-key"));
        assert!(!s.files.contains_key("openrouter-key"));
    }

    #[test]
    fn unset_not_set_nothing_to_unset() {
        let mut s = FakeStore::new();
        let r = run_config_logic(&argv(&["unset", "openrouter-key"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("nothing to unset"));
    }

    #[test]
    fn unset_unknown_key_exit_2() {
        let mut s = FakeStore::new();
        assert_eq!(
            run_config_logic(&argv(&["unset", "nope"]), &mut s, None).exit_code,
            2
        );
    }

    // --- path ---

    #[test]
    fn path_reports_backend_file_keys_never_values() {
        let mut s = FakeStore::new();
        s.files
            .insert("openrouter-key".into(), "sk-super-secret".into());
        let r = run_config_logic(&argv(&["path"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("Backend:     file"));
        assert!(r.stdout.contains("/quorum/qd/config.toml"));
        // B4 affordance: the key is listed WITH its resolving tier.
        assert!(r.stdout.contains("openrouter-key (file)"));
        assert!(!r.stdout.contains("sk-super-secret"));
    }

    #[test]
    fn path_empty_store_reports_none() {
        let mut s = FakeStore::new();
        assert!(run_config_logic(&argv(&["path"]), &mut s, None)
            .stdout
            .contains("(none)"));
    }

    #[test]
    fn path_extra_arg_exit_2() {
        let mut s = FakeStore::new();
        assert_eq!(
            run_config_logic(&argv(&["path", "extra"]), &mut s, None).exit_code,
            2
        );
    }

    // --- B4 item 1: the which-tier affordance (pure-logic rendering) ---

    /// A store that reports an explicit tier per key — proves the pure logic
    /// renders WHATEVER tier the store resolves (env / keychain / file), not a
    /// hardcoded label. This is the affordance's render contract.
    struct TieredStore {
        get_tier: Option<(String, String)>,
        keys: Vec<(String, String)>,
    }
    impl SecretStore for TieredStore {
        fn get(&self, _k: &str) -> Result<Option<(String, String)>, String> {
            Ok(self.get_tier.clone())
        }
        fn set(&mut self, _k: &str, _v: &str) -> Result<String, String> {
            Ok("file".to_string())
        }
        fn unset(&mut self, _k: &str) -> Result<(), String> {
            Ok(())
        }
        fn backend_info(&self) -> Result<BackendInfoTuple, String> {
            Ok((
                "keychain".to_string(),
                "/x/config.toml".to_string(),
                self.keys.clone(),
            ))
        }
    }

    #[test]
    fn get_surfaces_env_tier() {
        let mut s = TieredStore {
            get_tier: Some(("sk-or-v1-abcd1234".to_string(), "env".to_string())),
            keys: vec![],
        };
        let r = run_config_logic(&argv(&["get", "openrouter-key"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("(tier: env)"), "{}", r.stdout);
        // Value still masked by default (the tier label is not the secret).
        assert!(r.stdout.contains("••••1234"));
        assert!(!r.stdout.contains("sk-or-v1-abcd1234"));
    }

    /// THE STRAND-FIX VISIBILITY PIN (B4 item 1): a key the keychain selection
    /// would have missed, resolved from the FILE tier, is now listed by
    /// `config path` as `openrouter-key (file)` while the selected backend is
    /// keychain — was the misleading "(none)" / "unused while keychain is
    /// active".
    #[test]
    fn path_lists_file_stranded_key_with_tier_under_keychain_selection() {
        let mut s = TieredStore {
            get_tier: None,
            keys: vec![("openrouter-key".to_string(), "file".to_string())],
        };
        let r = run_config_logic(&argv(&["path"]), &mut s, None);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("Backend:     keychain"));
        assert!(
            r.stdout.contains("openrouter-key (file)"),
            "file-stranded key must show tier=file: {}",
            r.stdout
        );
        assert!(
            !r.stdout.contains("unused while keychain is active"),
            "the misleading file-note must be gone: {}",
            r.stdout
        );
    }

    #[test]
    fn get_reveal_shows_value_and_tier() {
        let mut s = TieredStore {
            get_tier: Some(("sk-or-v1-abcd1234".to_string(), "keychain".to_string())),
            keys: vec![],
        };
        let r = run_config_logic(&argv(&["get", "openrouter-key", "--reveal"]), &mut s, None);
        assert!(r.stdout.contains("sk-or-v1-abcd1234"));
        assert!(r.stdout.contains("(tier: keychain)"));
    }
}
