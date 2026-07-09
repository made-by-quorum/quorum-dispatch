//! Tiered secret store for `qd` — port of `0d0fa9e:src/secrets.ts`.
//!
//! Goal: stop users hand-exporting OPENROUTER_API_KEY. Secrets live in a tiered
//! backend — the macOS Keychain when available, otherwise a chmod-600 config
//! file under `~/.quorum/dispatch`. An env var ALWAYS overrides both (so CI / one-offs / the
//! existing survey contract keep working unchanged).
//! (`0d0fa9e:src/secrets.ts:3-7`).
//!
//! TESTABILITY (mirrors the house seam pattern — `Exec`/`Env`): every real
//! effect is INJECTED via [`SecretDeps`]. Tests assert the `security` argv
//! construction + parse fake output (NEVER invoking real `security`), and
//! exercise the file backend against an injected fs (NEVER writing real files,
//! NEVER touching the real Keychain). Backend selection is itself injectable
//! (platform + a keychain-probe), so platform-specific paths are
//! deterministically unit-tested on any host. (`0d0fa9e:src/secrets.ts:9-15`).
//!
//! A5 DIVERGENCE (ADR 0010, `keychain-fallback`): when the keychain backend is
//! SELECTED (not env-forced) and a write/read hits the locked / no-interaction
//! signature (`User interaction is not allowed`, the documented
//! errSecInteractionNotAllowed text — inbox bug 2026-06-04), the op retries on
//! the FILE backend with a one-per-process stderr notice. The TS has no
//! fallback; its headless `config set` is BROKEN (the inbox bug). See §3.2 of
//! the A5 spec + ADR 0010.

use crate::effects::Env;
use crate::exec::{Exec, ExecResult};
use std::sync::atomic::{AtomicBool, Ordering};

// ----------------------------------------------------------------------------
// Key registry (name -> env var). Small + extensible.
// (`0d0fa9e:src/secrets.ts:21-37`).
// ----------------------------------------------------------------------------

/// Known secret names and the env var that overrides each. Keep this small; add
/// a row to register a new key. `qd config` rejects any name not listed here.
/// (`0d0fa9e:src/secrets.ts:25-27`).
pub const KNOWN_KEYS: &[(&str, &str)] = &[("openrouter-key", "OPENROUTER_API_KEY")];

/// PLAIN (non-secret) config keys (qb punch item 7). These live ONLY in the
/// file tier — as TOP-LEVEL `key = "value"` lines in `~/.quorum/dispatch/config.toml` (the
/// `claude_flags` precedent), NEVER the keychain (they are not secrets) and
/// NEVER the `[secrets]` table. `qd config set/get/unset` accepts them like any
/// known key but routes them through the plain top-level read/write below.
pub const PLAIN_FILE_KEYS: &[&str] = &["render-default"];

/// Is `name` a plain (file-tier, non-secret) config key?
pub fn is_plain_file_key(name: &str) -> bool {
    PLAIN_FILE_KEYS.contains(&name)
}

/// Sorted list of known key names (`0d0fa9e:src/secrets.ts:29-31`), now
/// including the plain file-tier keys (punch item 7).
pub fn known_key_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = KNOWN_KEYS.iter().map(|(n, _)| *n).collect();
    v.extend(PLAIN_FILE_KEYS.iter().copied());
    v.sort_unstable();
    v
}

/// Is `name` a registered key? (`0d0fa9e:src/secrets.ts:33-35`).
pub fn is_known_key(name: &str) -> bool {
    KNOWN_KEYS.iter().any(|(n, _)| *n == name) || is_plain_file_key(name)
}

/// The env var that overrides a given key, if known. Plain file-tier keys have
/// NO env override (None).
pub fn env_var_for_key(name: &str) -> Option<&'static str> {
    KNOWN_KEYS.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
}

/// Validate a value for a known key at `qd config set` time (punch item 7,
/// errors-that-teach). `None` = acceptable; `Some(msg)` = the teaching error.
/// Secret keys accept anything (a secret's shape is the backend's business).
pub fn validate_key_value(key: &str, value: &str) -> Option<String> {
    if key == "render-default" && value != "inline" && value != "alt-screen" {
        return Some(format!(
            "qd config set: invalid value '{value}' for render-default — use 'inline' \
             (the default: sessions render in the scrollback so phone/SSH attach can \
             scroll) or 'alt-screen' (fullscreen rendering; opt back in per-session \
             with --alt-screen)."
        ));
    }
    None
}

/// The Keychain service all qd secrets share; the account is the secret name.
/// (`0d0fa9e:src/secrets.ts:38`).
pub const KEYCHAIN_SERVICE: &str = "qd-cli";

/// The locked-keychain detection signature (ADR 0010; A5 spec §3.2). The
/// documented errSecInteractionNotAllowed (OSStatus -25308) text, AND the
/// literal string in the live failure the inbox bug captured. STRING-MATCH
/// ONLY — the live exit code (36) is corroborating, NOT a detection key.
const LOCKED_KEYCHAIN_SIGNATURE: &str = "User interaction is not allowed";

/// The one-per-process fallback notice (ADR 0010; A5 spec §3.2).
const FALLBACK_NOTICE: &str =
    "qd config: keychain locked (headless?) — falling back to file backend (~/.quorum/dispatch/config.toml).";

/// The one-per-process env-forced-locked GET diagnostic (orc-2 ruling
/// relay-1780639217973-4, "middle path c"; A5 spec §3.2). Under env-forced
/// `QD_SECRET_BACKEND=keychain`, a GET that hits the locked signature keeps the
/// TS-parity stdout/exit (`<key>: not set.`, exit 0 — presence-probing scripts
/// stay unbroken) but emits THIS single attributable stderr line so the operator
/// is not left conflating ABSENT with INACCESSIBLE (ADD-9a: a TS diagnostic
/// deficiency we do not reproduce). Diagnostic-stderr-only divergence; stdout +
/// exit stay TS-parity.
const ENV_FORCED_LOCKED_DIAG: &str =
    "warning: keychain is locked — a key may exist but is inaccessible (QD_SECRET_BACKEND=keychain is env-forced; unlock or unset to use fallback).";

// ----------------------------------------------------------------------------
// Injected effects (`0d0fa9e:src/secrets.ts:41-74`).
// ----------------------------------------------------------------------------

/// Read the config file's text, or `None` if it does not exist.
pub type ReadFile<'a> = dyn Fn(&str) -> Option<String> + 'a;
/// Write text to the config file (creating it).
pub type WriteFile<'a> = dyn Fn(&str, &str) + 'a;
/// chmod a path (used to enforce 600 on the config file).
pub type Chmod<'a> = dyn Fn(&str, u32) + 'a;
/// Does the config file currently exist?
pub type FileExists<'a> = dyn Fn(&str) -> bool + 'a;

/// The injected effects bag (the Rust analogue of TS `SecretDeps`,
/// `0d0fa9e:src/secrets.ts:53-74`). Every real side effect is a seam so unit
/// tests are hermetic.
///
/// `exec` + `keychain_available` drive the keychain backend; the four fs
/// closures drive the file backend; `env` carries the override precedence +
/// QD_HOME resolution + the `QD_SECRET_BACKEND` selector.
pub struct SecretDeps<'a> {
    /// Host platform string (`std::env::consts::OS`-shaped: "macos" / "linux").
    /// Inject to force a backend. (TS `platform: NodeJS.Platform`.)
    pub platform: &'a str,
    /// Env, for the override precedence + QD_HOME resolution.
    pub env: &'a dyn Env,
    /// Run `security ...` argv. Injected so tests assert argv. (TS `exec`.)
    pub exec: &'a dyn Exec,
    /// Does the keychain backend work on this host? (probed once, injectable).
    pub keychain_available: &'a dyn Fn() -> bool,
    /// Read the config file's text, or `None` if it does not exist.
    pub read_file: &'a ReadFile<'a>,
    /// Write text to the config file (creating it).
    pub write_file: &'a WriteFile<'a>,
    /// chmod a path (used to enforce 600 on the config file).
    pub chmod: &'a Chmod<'a>,
    /// Does the config file currently exist?
    pub file_exists: &'a FileExists<'a>,
    /// One-per-PROCESS guard for the fallback notice (A5 §3.2: notice printed
    /// ONCE per process even though `backend_info` does per-key gets).
    pub fallback_notice_emitted: &'a AtomicBool,
    /// One-per-PROCESS guard for the env-forced-locked GET diagnostic (orc-2
    /// ruling relay-1780639217973-4; A5 §3.2). SEPARATE from
    /// `fallback_notice_emitted` so the two divergence lines never share a flag:
    /// the env-forced-locked path emits ONE attributable line even though
    /// `secret_backend_info` does a per-key get for every known key (without a
    /// separate guard those per-key gets would each print, spamming N lines).
    pub locked_diag_emitted: &'a AtomicBool,
}

/// `darwin` is TS's macOS platform tag; Rust's `std::env::consts::OS` is
/// `"macos"`. Backend selection treats EITHER as macOS so injected tests can use
/// the TS tag and production uses the Rust tag.
fn is_macos(platform: &str) -> bool {
    platform == "macos" || platform == "darwin"
}

// ----------------------------------------------------------------------------
// File backend: ~/.quorum/dispatch/config.toml, [secrets] table, chmod 600.
// (`0d0fa9e:src/secrets.ts:77-163`).
// ----------------------------------------------------------------------------

/// Resolve `~/.quorum/dispatch` (QD_HOME -> default `~/.quorum/dispatch`), matching resolveBootstrapPaths.
/// (`0d0fa9e:src/secrets.ts:80-83`).
pub fn resolve_qd_home(env: &dyn Env) -> String {
    if let Some(qd_home) = env.var("QD_HOME") {
        return qd_home;
    }
    let home = env.var("HOME").unwrap_or_default();
    format!("{home}/.quorum/dispatch")
}

/// The config-file path for the file backend. (`0d0fa9e:src/secrets.ts:85-87`).
pub fn resolve_config_path(env: &dyn Env) -> String {
    format!("{}/config.toml", resolve_qd_home(env))
}

/// Parse the `[secrets]` table out of the config file. A deliberately tiny,
/// purpose-built parser for our own flat string-table format (NOT a general
/// TOML parser): a `[secrets]` header followed by `key = "value"` lines. Unknown
/// sections/lines are ignored (permissive read, L8).
/// (`0d0fa9e:src/secrets.ts:89-107`).
pub fn parse_secrets_toml(text: &str) -> Vec<(String, String)> {
    parse_toml_section(text, "secrets")
}

/// Parse ANY single `[name]` section's `key = "value"` rows out of our flat
/// string-table config format. [`parse_secrets_toml`] is this specialized to
/// `"secrets"`; `crate::archive::config` reuses it for `[archive]` rather than
/// re-deriving the same tiny parser for a second table.
pub(crate) fn parse_toml_section(text: &str, section: &str) -> Vec<(String, String)> {
    let mut table: Vec<(String, String)> = Vec::new();
    let mut in_section = false;
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Section header `[name]`.
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = name.trim() == section;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((key, value)) = parse_kv_line(line) {
            // Last write wins (mirrors JS object assignment).
            table.retain(|(k, _)| k != &key);
            table.push((key, value));
        }
    }
    table
}

/// Parse one `key = "value"` line, matching the TS regex
/// `^([A-Za-z0-9_-]+)\s*=\s*"((?:[^"\\]|\\.)*)"$` (`0d0fa9e:src/secrets.ts:104`).
/// Returns the unescaped value. None if the line doesn't match the strict shape.
fn parse_kv_line(line: &str) -> Option<(String, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let rhs = line[eq + 1..].trim();
    // RHS must be a fully-quoted string with no trailing garbage.
    let bytes = rhs.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    // Walk the quoted body honoring backslash escapes; the closing quote must be
    // the LAST char (no trailing content), matching the anchored TS regex.
    let mut out = String::new();
    let mut i = 1;
    let chars: Vec<char> = rhs.chars().collect();
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            // `\\.` — the escaped char is taken literally after unescaping.
            if i + 1 >= chars.len() {
                return None; // dangling backslash → no closing quote → no match
            }
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '"' {
            // Closing quote: must be the final char.
            return if i == chars.len() - 1 {
                Some((key.to_string(), out))
            } else {
                None
            };
        }
        out.push(c);
        i += 1;
    }
    None // ran off the end without a closing quote
}

/// Escape a value for the config file (`0d0fa9e:src/secrets.ts:109-111`):
/// backslash then double-quote.
fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The managed header comment line `serialize_secrets_toml` emits (and the
/// merge in [`write_secrets_table`] dedupes against).
const MANAGED_HEADER: &str =
    "# qd config -- managed by `qd config`. Do not hand-edit secret values.";

/// Serialize a `[secrets]` table back to config-file text. Keys sorted for a
/// stable, diff-friendly file. Byte-for-byte the TS emitter
/// (`0d0fa9e:src/secrets.ts:117-125`): header comment, `[secrets]`, sorted
/// `name = "value"` lines, trailing newline.
pub fn serialize_secrets_toml(table: &[(String, String)]) -> String {
    let mut names: Vec<&(String, String)> = table.iter().collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let mut lines: Vec<String> = vec![MANAGED_HEADER.to_string(), "[secrets]".to_string()];
    for (name, value) in names {
        lines.push(format!("{name} = \"{}\"", escape_toml(value)));
    }
    format!("{}\n", lines.join("\n"))
}

fn read_secrets_table(deps: &SecretDeps) -> Vec<(String, String)> {
    let path = resolve_config_path(deps.env);
    match (deps.read_file)(&path) {
        None => Vec::new(),
        Some(text) => parse_secrets_toml(&text),
    }
}

/// Merge a re-serialized `[secrets]` section INTO the existing config text,
/// PRESERVING everything outside the managed section (punch item 7): top-level
/// plain keys (`render-default`, `claude_flags`), hand comments, and any other
/// `[section]` — previously a file-backend secret write replaced the WHOLE
/// file with header + `[secrets]`, silently clobbering top-level keys. With no
/// preserved content the output is byte-identical to `serialize_secrets_toml`
/// (the TS-parity shape — pinned).
fn merge_secrets_into_config(existing: &str, table: &[(String, String)]) -> String {
    enum Zone {
        Top,
        Secrets,
        Other,
    }
    let mut top: Vec<&str> = Vec::new();
    let mut others: Vec<&str> = Vec::new();
    let mut zone = Zone::Top;
    for line in existing.trim_end_matches('\n').split('\n') {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if name.trim() == "secrets" {
                zone = Zone::Secrets; // body re-serialized from `table`
            } else {
                zone = Zone::Other;
                others.push(line);
            }
            continue;
        }
        match zone {
            // Top-level region: keep everything except the managed header
            // (re-emitted by the serializer) and blank lines (cosmetic).
            Zone::Top => {
                if trimmed != MANAGED_HEADER && !trimmed.is_empty() {
                    top.push(line);
                }
            }
            Zone::Secrets => {}
            Zone::Other => others.push(line),
        }
    }
    let serialized = serialize_secrets_toml(table);
    if top.is_empty() && others.is_empty() {
        return serialized; // byte-identical to the pre-merge shape
    }
    // Splice: header line, preserved top-level lines, then the serializer's
    // body ([secrets] + rows), then the other sections. ONE emitter — the
    // body is taken from `serialized` after its header line.
    // S5: loud beats silent-wrong — a serializer that stopped emitting the
    // managed header would otherwise get it silently DUPLICATED here.
    let body = serialized
        .strip_prefix(MANAGED_HEADER)
        .expect("serializer emits the managed header")
        .trim_start_matches('\n');
    let mut out = String::with_capacity(existing.len() + serialized.len());
    out.push_str(MANAGED_HEADER);
    out.push('\n');
    for l in &top {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(body);
    for l in &others {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn write_secrets_table(deps: &SecretDeps, table: &[(String, String)]) {
    let path = resolve_config_path(deps.env);
    let existing = (deps.read_file)(&path).unwrap_or_default();
    (deps.write_file)(&path, &merge_secrets_into_config(&existing, table));
    // Enforce 600 on EVERY write (covers both create + update of an existing
    // file): secrets must never be group/other-readable.
    // (`0d0fa9e:src/secrets.ts:131-133`).
    (deps.chmod)(&path, 0o600);
}

fn file_get(name: &str, deps: &SecretDeps) -> Option<String> {
    read_secrets_table(deps)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

fn file_set(name: &str, value: &str, deps: &SecretDeps) {
    let mut table = read_secrets_table(deps);
    table.retain(|(k, _)| k != name);
    table.push((name.to_string(), value.to_string()));
    write_secrets_table(deps, &table);
}

fn file_delete(name: &str, deps: &SecretDeps) {
    let mut table = read_secrets_table(deps);
    if !table.iter().any(|(k, _)| k == name) {
        return;
    }
    table.retain(|(k, _)| k != name);
    write_secrets_table(deps, &table);
}

// ----------------------------------------------------------------------------
// Plain file-tier keys (punch item 7): TOP-LEVEL `key = "value"` lines in the
// SAME config.toml, OUTSIDE any section (the `claude_flags` precedent). The
// writer is a line-level upsert that preserves every other byte of the file
// (comments, claude_flags, the [secrets] table) — it never re-serializes.
// ----------------------------------------------------------------------------

/// Read a TOP-LEVEL (pre-any-section) `name = "value"` key from config text.
/// Strict quoted shape (the same `parse_kv_line` the [secrets] table uses);
/// lines inside any `[section]` are never considered.
pub fn get_plain_config_key(text: &str, name: &str) -> Option<String> {
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.starts_with('[') {
            // First section header ends the top-level region.
            return None;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = parse_kv_line(line) {
            if key == name {
                return Some(value);
            }
        }
    }
    None
}

/// Upsert (value = `Some`) or remove (value = `None`) a TOP-LEVEL key in config
/// text, preserving every other line byte-for-byte. An existing top-level line
/// for `name` is replaced in place; a missing key is inserted at the END of the
/// top-level region (just before the first section header, or appended when the
/// file has no sections).
fn upsert_top_level_key(text: &str, name: &str, value: Option<&str>) -> String {
    let mut lines: Vec<String> = if text.is_empty() {
        Vec::new()
    } else {
        text.trim_end_matches('\n')
            .split('\n')
            .map(|s| s.to_string())
            .collect()
    };
    // The top-level region = lines before the first `[section]` header.
    let first_header = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    // An existing top-level line for `name` (strict quoted kv shape)?
    let existing = lines[..first_header]
        .iter()
        .position(|l| matches!(parse_kv_line(l.trim()), Some((k, _)) if k == name));
    match (existing, value) {
        (Some(i), Some(v)) => lines[i] = format!("{name} = \"{}\"", escape_toml(v)),
        (Some(i), None) => {
            lines.remove(i);
        }
        (None, Some(v)) => lines.insert(first_header, format!("{name} = \"{}\"", escape_toml(v))),
        (None, None) => {}
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn plain_file_get(name: &str, deps: &SecretDeps) -> Option<String> {
    let path = resolve_config_path(deps.env);
    let text = (deps.read_file)(&path)?;
    get_plain_config_key(&text, name)
}

fn plain_file_set(name: &str, value: &str, deps: &SecretDeps) {
    let path = resolve_config_path(deps.env);
    let existing = (deps.read_file)(&path).unwrap_or_default();
    (deps.write_file)(&path, &upsert_top_level_key(&existing, name, Some(value)));
    // The file may also hold secrets — keep it 0600 on every write.
    (deps.chmod)(&path, 0o600);
}

fn plain_file_delete(name: &str, deps: &SecretDeps) {
    let path = resolve_config_path(deps.env);
    let Some(existing) = (deps.read_file)(&path) else {
        return;
    };
    if get_plain_config_key(&existing, name).is_none() {
        return;
    }
    (deps.write_file)(&path, &upsert_top_level_key(&existing, name, None));
    (deps.chmod)(&path, 0o600);
}

// ----------------------------------------------------------------------------
// Keychain backend: `security` CLI, service=qd-cli, account=<name>.
// (`0d0fa9e:src/secrets.ts:165-200`).
// ----------------------------------------------------------------------------

/// `security add-generic-password` argv (`0d0fa9e:src/secrets.ts:167-170`).
/// `-U` updates an existing item instead of erroring; `-w` passes the value.
pub fn keychain_set_argv(name: &str, value: &str) -> Vec<String> {
    vec![
        "add-generic-password".into(),
        "-U".into(),
        "-a".into(),
        name.into(),
        "-s".into(),
        KEYCHAIN_SERVICE.into(),
        "-w".into(),
        value.into(),
    ]
}

/// `security find-generic-password` argv (`0d0fa9e:src/secrets.ts:172-175`).
/// `-w` prints ONLY the password to stdout; non-zero exit => not found.
pub fn keychain_get_argv(name: &str) -> Vec<String> {
    vec![
        "find-generic-password".into(),
        "-a".into(),
        name.into(),
        "-s".into(),
        KEYCHAIN_SERVICE.into(),
        "-w".into(),
    ]
}

/// `security delete-generic-password` argv (`0d0fa9e:src/secrets.ts:177-179`).
pub fn keychain_delete_argv(name: &str) -> Vec<String> {
    vec![
        "delete-generic-password".into(),
        "-a".into(),
        name.into(),
        "-s".into(),
        KEYCHAIN_SERVICE.into(),
    ]
}

/// Run a `security` argv through the exec seam, returning the captured result.
/// `security` is resolved on PATH (the binary name, not an absolute path) —
/// the keychain backend is only selected when `keychain_available()` already
/// confirmed it is invocable.
fn run_security(argv: &[String], deps: &SecretDeps) -> ExecResult {
    match deps.exec.run("security", argv, &[], None, None) {
        Ok(r) => r,
        // A spawn error (ENOENT etc.) is shaped as an exit-1 failure with the io
        // error on stderr; the not-set / signature logic handles it like any
        // non-zero `security` exit.
        Err(e) => ExecResult {
            status: Some(1),
            stdout: String::new(),
            stderr: e.to_string(),
            timed_out: false,
        },
    }
}

/// Does this `security` result carry the locked / no-interaction signature?
/// STRING-MATCH ONLY on stderr (ADR 0010; the exit code is NOT consulted).
fn is_locked_signature(r: &ExecResult) -> bool {
    r.stderr.contains(LOCKED_KEYCHAIN_SIGNATURE)
}

/// A keychain operation outcome that distinguishes the locked-fallback signal
/// from ordinary success/failure, so callers can decide whether to fall back.
enum KeychainOutcome<T> {
    /// The op completed (value / unit / etc.).
    Ok(T),
    /// The op hit the locked / no-interaction signature → caller may fall back.
    Locked,
    /// A non-signature failure (keeps TS semantics — get treats as not-set, set
    /// surfaces the error). Carries the error message for `set`.
    Failed(String),
}

/// Keychain GET with fallback signalling (`0d0fa9e:src/secrets.ts:181-186`,
/// extended for ADR 0010). Returns the found value, `Locked` on the signature,
/// or `Ok(None)` for an ordinary not-found.
fn keychain_get(name: &str, deps: &SecretDeps) -> KeychainOutcome<Option<String>> {
    let r = run_security(&keychain_get_argv(name), deps);
    if r.status != Some(0) {
        // Non-zero exit => not found (TS semantics) UNLESS it is the locked
        // signature, in which case the caller falls back.
        if is_locked_signature(&r) {
            return KeychainOutcome::Locked;
        }
        return KeychainOutcome::Ok(None);
    }
    // `security -w` emits the password + a trailing newline; nothing else.
    let value = r.stdout.strip_suffix('\n').unwrap_or(&r.stdout).to_string();
    KeychainOutcome::Ok(if value.is_empty() { None } else { Some(value) })
}

/// Keychain SET with fallback signalling (`0d0fa9e:src/secrets.ts:188-193`,
/// extended for ADR 0010).
fn keychain_set(name: &str, value: &str, deps: &SecretDeps) -> KeychainOutcome<()> {
    let r = run_security(&keychain_set_argv(name, value), deps);
    if r.status == Some(0) {
        return KeychainOutcome::Ok(());
    }
    if is_locked_signature(&r) {
        return KeychainOutcome::Locked;
    }
    let code = r.status.unwrap_or(1);
    let trimmed = r.stderr.trim();
    let msg = if trimmed.is_empty() {
        format!("qd config: keychain write failed (exit {code})")
    } else {
        format!("qd config: keychain write failed (exit {code}) -- {trimmed}")
    };
    KeychainOutcome::Failed(msg)
}

/// Keychain DELETE (`0d0fa9e:src/secrets.ts:195-198`). A non-zero exit means
/// "no such item" — idempotent delete, not an error.
fn keychain_delete(name: &str, deps: &SecretDeps) {
    run_security(&keychain_delete_argv(name), deps);
}

// ----------------------------------------------------------------------------
// Backend selection (`0d0fa9e:src/secrets.ts:202-219`).
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    File,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Keychain => "keychain",
            Backend::File => "file",
        }
    }
}

/// Is the keychain backend EXPLICITLY forced via `QD_SECRET_BACKEND=keychain`?
/// Env-forced keychain NEVER falls back (ADR 0010 / A5 §3.2 — explicit operator
/// intent fails loud).
fn is_keychain_env_forced(deps: &SecretDeps) -> bool {
    deps.env.var("QD_SECRET_BACKEND").as_deref() == Some("keychain")
}

/// Which backend is active: keychain on macOS when `security` works, else file.
/// `QD_SECRET_BACKEND=file|keychain` forces a backend.
/// (`0d0fa9e:src/secrets.ts:208-219`).
///
/// PORT NOTE (A5 §3.1): the TS only honors exactly `"file"` / `"keychain"`; any
/// OTHER value of `QD_SECRET_BACKEND` is SILENTLY IGNORED (falls through to the
/// platform default). Verified against the pin — there is no notice on an
/// invalid value. We match that exactly (matrix note: invalid value = silent
/// fall-through, no stderr).
pub fn select_backend(deps: &SecretDeps) -> Backend {
    match deps.env.var("QD_SECRET_BACKEND").as_deref() {
        Some("file") => return Backend::File,
        Some("keychain") => return Backend::Keychain,
        _ => {}
    }
    if is_macos(deps.platform) && (deps.keychain_available)() {
        Backend::Keychain
    } else {
        Backend::File
    }
}

/// Emit the one-per-process fallback notice to stderr (ADR 0010 / A5 §3.2).
fn emit_fallback_notice_once(deps: &SecretDeps) {
    // `swap` returns the PREVIOUS value: only the first caller (false -> true)
    // prints. Acquire/Release pairing is overkill for a flag but cheap +
    // correct.
    if !deps.fallback_notice_emitted.swap(true, Ordering::SeqCst) {
        eprintln!("{FALLBACK_NOTICE}");
    }
}

/// Emit the one-per-process env-forced-locked GET diagnostic to stderr (orc-2
/// ruling relay-1780639217973-4; A5 §3.2). Same swap-flag pattern as
/// [`emit_fallback_notice_once`], its OWN flag so a `backend_info` sweep prints
/// the line ONCE, not once per known key.
fn emit_locked_diag_once(deps: &SecretDeps) {
    if !deps.locked_diag_emitted.swap(true, Ordering::SeqCst) {
        eprintln!("{ENV_FORCED_LOCKED_DIAG}");
    }
}

// ----------------------------------------------------------------------------
// Public API (name-based; backend chosen per host).
// (`0d0fa9e:src/secrets.ts:221-260`).
// ----------------------------------------------------------------------------

/// Read a secret from the active backend (None = not set).
/// (`0d0fa9e:src/secrets.ts:223-225`, extended for ADR 0010 keychain fallback.)
///
/// qb punch B4 item 1 (TIER-STRANDING fix, orc-ratified read-side fallthrough):
/// a keychain-SELECTED read that cleanly MISSES (unlocked, item absent) now
/// FALLS THROUGH to the file backend instead of reporting "not set". The strand:
/// writers legitimately land file-tier values (ADR-0010 locked-set fallback;
/// `QD_SECRET_BACKEND=file qd config set …` — which the engine's own non-TTY
/// hint recommends; hand-edit), and a later unlocked-keychain read missed them
/// for every store consumer at once. Keychain still WINS when present (a stale
/// file copy never shadows a live keychain value — the file answers ONLY on a
/// clean keychain miss). Read-side only: `set_secret`/`delete_secret` are
/// UNCHANGED (no set-side cross-tier self-heal — orc ruling).
pub fn get_secret(name: &str, deps: &SecretDeps) -> Option<String> {
    // Plain file-tier keys (punch item 7) NEVER touch the keychain.
    if is_plain_file_key(name) {
        return plain_file_get(name, deps);
    }
    if select_backend(deps) == Backend::Keychain {
        match keychain_get(name, deps) {
            // B4 item 1: a clean keychain HIT wins; a clean keychain MISS
            // (`Ok(None)`) or a non-signature failure now FALLS THROUGH to the
            // file tier (was: return None — the strand).
            KeychainOutcome::Ok(Some(v)) => Some(v),
            KeychainOutcome::Ok(None) | KeychainOutcome::Failed(_) => file_get(name, deps),
            KeychainOutcome::Locked => {
                // Env-forced keychain never falls back. stdout/exit stay
                // TS-parity (`<key>: not set.`, exit 0 — presence-probing scripts
                // stay unbroken; the caller maps None to that line), BUT we emit
                // ONE attributable stderr diagnostic so the operator who pinned
                // the backend is not left conflating ABSENT with INACCESSIBLE
                // (orc-2 ruling relay-1780639217973-4 "middle path c"; ADD-9a —
                // a TS diagnostic deficiency we do not reproduce). Once per
                // process (own flag) so a `backend_info` sweep prints one line.
                if is_keychain_env_forced(deps) {
                    emit_locked_diag_once(deps);
                    return None;
                }
                emit_fallback_notice_once(deps);
                file_get(name, deps)
            }
        }
    } else {
        file_get(name, deps)
    }
}

/// Store a secret in the active backend. Returns the backend that ACTUALLY
/// stored the value (truthful under fallback — A5 §3.2). Err = a non-signature
/// keychain failure (kept loud, TS semantics) OR env-forced-keychain lock.
/// (`0d0fa9e:src/secrets.ts:227-230`, extended for ADR 0010.)
pub fn set_secret(name: &str, value: &str, deps: &SecretDeps) -> Result<Backend, String> {
    // Plain file-tier keys (punch item 7) NEVER touch the keychain — they are
    // not secrets and the launch path reads them from the file.
    if is_plain_file_key(name) {
        plain_file_set(name, value, deps);
        return Ok(Backend::File);
    }
    if select_backend(deps) == Backend::Keychain {
        match keychain_set(name, value, deps) {
            KeychainOutcome::Ok(()) => Ok(Backend::Keychain),
            KeychainOutcome::Failed(msg) => Err(msg),
            KeychainOutcome::Locked => {
                if is_keychain_env_forced(deps) {
                    // Env-forced keychain fails LOUD (ADR 0010 / A5 §3.2): the
                    // operator explicitly demanded keychain; do NOT silently
                    // write a weaker file copy. Surface the locked failure.
                    return Err(format!(
                        "qd config: keychain locked ({LOCKED_KEYCHAIN_SIGNATURE}) and QD_SECRET_BACKEND=keychain forbids file fallback. Unlock the keychain or use QD_SECRET_BACKEND=file."
                    ));
                }
                emit_fallback_notice_once(deps);
                file_set(name, value, deps);
                Ok(Backend::File)
            }
        }
    } else {
        file_set(name, value, deps);
        Ok(Backend::File)
    }
}

/// Delete from WHICHEVER backend holds it. Try the active backend; on the file
/// backend also clear any stale file copy, and on keychain a delete is
/// idempotent. This keeps `unset` honest even if a secret was set under a
/// different backend earlier (e.g. file fallback, then keychain became
/// available). (`0d0fa9e:src/secrets.ts:232-242`).
pub fn delete_secret(name: &str, deps: &SecretDeps) {
    // Plain file-tier keys (punch item 7): file-only, keychain never consulted.
    if is_plain_file_key(name) {
        plain_file_delete(name, deps);
        return;
    }
    if select_backend(deps) == Backend::Keychain {
        keychain_delete(name, deps);
    }
    // ALWAYS also clear the file copy if present (covers backend switchover AND
    // the ADR 0010 locked-fallback case where the value landed in the file).
    if (deps.file_exists)(&resolve_config_path(deps.env)) {
        file_delete(name, deps);
    }
}

/// Backend label + config path + which keys are set (names only, never values).
/// (`0d0fa9e:src/secrets.ts:244-262`).
#[derive(Debug, PartialEq)]
pub struct BackendInfo {
    pub backend: Backend,
    /// The config-file path (always reported, even on keychain, for
    /// transparency).
    pub file_path: String,
    /// Keys currently set, each with the TIER it resolves from (B4 item 1
    /// affordance) — never values. Sorted by key name.
    pub keys_set: Vec<(String, Source)>,
}

/// Report the selected backend, the file path, and the keys that are set —
/// each tagged with the TIER it resolves from (B4 item 1 affordance).
///
/// A5 §3.2: the `backend` field is the SELECTION (keychain stays keychain even
/// under fallback). The keys-set enumeration resolves each known key through
/// [`resolve_config_tier`] (the same precedence `qd config get` reports), so a
/// file-stranded key now truthfully shows tier=`File` — was the misleading
/// "(none)" while config.toml held it. `config path`'s `Backend:` line reports
/// the selection; `Keys set:` reports the effective per-tier state.
pub fn secret_backend_info(deps: &SecretDeps) -> BackendInfo {
    let backend = select_backend(deps);
    let file_path = resolve_config_path(deps.env);
    // B4 item 1: report each set key WITH the tier it resolves from, across the
    // full env > keychain > file precedence (`resolve_config_tier`). Two source
    // sets are unioned:
    //   (a) every KNOWN key — so an env-exported key (empty store) lists as
    //       (env), a keychain key as (keychain), a file-stranded key as (file);
    //   (b) any STRAY hand-added `[secrets]` key in the file — names NOT in the
    //       registry (S1(ii)): the old code enumerated the whole table, so
    //       dropping to known-only would silently hide a key the operator hand
    //       added, defeating "which keys are set" on a transparency surface.
    //       Stray keys are file-tier by construction → tier=File.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut keys_set: Vec<(String, Source)> = Vec::new();
    for n in known_key_names() {
        if let Some(s) = resolve_config_tier(n, deps).source {
            if seen.insert(n.to_string()) {
                keys_set.push((n.to_string(), s));
            }
        }
    }
    for (k, _) in read_secrets_table(deps) {
        if !is_known_key(&k) && seen.insert(k.clone()) {
            keys_set.push((k, Source::File));
        }
    }
    // Stable, diff-friendly order (the union of two sources is not pre-sorted).
    keys_set.sort_by(|a, b| a.0.cmp(&b.0));
    BackendInfo {
        backend,
        file_path,
        keys_set,
    }
}

// ----------------------------------------------------------------------------
// Resolution precedence (used by survey): env var -> active backend.
// (`0d0fa9e:src/secrets.ts:264-297`).
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Keychain,
    File,
}

impl Source {
    /// The which-tier affordance label (B4 item 1). `None` (absent) renders as
    /// "unset" at the call site — this method is only the present-tier word.
    pub fn label(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Keychain => "keychain",
            Source::File => "file",
        }
    }
}

/// Wrap a file-backend read into a [`ResolvedSecret`] (B4 S2 dedup): `Some` →
/// `source = File`, `None` → absent. The single shape for "the value (if any)
/// lives in the file tier" — used by every file-resolving arm (clean-miss
/// fallthrough, locked fallback, the file-backend tail, the plain-key branch)
/// so the `File`-source construction can never drift between them.
fn resolved_file(v: Option<String>) -> ResolvedSecret {
    match v {
        Some(value) => ResolvedSecret {
            value: Some(value),
            source: Some(Source::File),
            locked: false,
        },
        None => ResolvedSecret {
            value: None,
            source: None,
            locked: false,
        },
    }
}

/// Resolve a CONFIG key (secret OR plain file-tier) through the full precedence
/// for the which-tier affordance (B4 item 1): `env > keychain > file` for
/// secret keys; file-only for plain keys (`render-default` — no env, no
/// keychain). Returns the value + the tier it resolved from (`None` = unset).
///
/// This is the single resolution `qd config get` / `qd config path` report
/// from, so the operator sees WHERE a value came from — converting tier drift
/// from archaeology to a one-line diagnosis, and closing the
/// config-get-vs-survey asymmetry (config now consults the env tier for the
/// label, exactly as survey does). The VALUE is masked by the caller per the
/// `--reveal` discipline; the TIER label is not a secret.
pub fn resolve_config_tier(name: &str, deps: &SecretDeps) -> ResolvedSecret {
    if is_plain_file_key(name) {
        // Plain keys live only in the file tier (no env override by design).
        return resolved_file(plain_file_get(name, deps));
    }
    // Secret keys: the shared env > keychain > file precedence. The env-var
    // name is the registry's; a key with no env var (none today) resolves
    // store-only.
    let env_var = env_var_for_key(name).unwrap_or("");
    resolve_secret(name, env_var, deps)
}

#[derive(Debug, PartialEq)]
pub struct ResolvedSecret {
    pub value: Option<String>,
    /// Where it came from; None when absent.
    pub source: Option<Source>,
    /// True ONLY when the value is absent because an env-forced
    /// (`QD_SECRET_BACKEND=keychain`) keychain was LOCKED — i.e. the null is
    /// INACCESSIBLE, not ABSENT. Lets callers (survey, M5) distinguish "no key
    /// configured" from "a key may exist but the locked keychain hid it" and
    /// report accordingly (orc-2 ruling relay-1780639217973-4: "a richer resolve
    /// API so survey can distinguish absent/locked"). `false` in every other
    /// outcome, including the non-env-forced fallback (there the value is read
    /// from the file, so it is not inaccessible).
    pub locked: bool,
}

/// Resolve a secret with full precedence: `env > keychain > file`. The env var
/// ALWAYS wins; then the active backend (keychain), then the file tier.
///
/// B4 item 1 (orc-ratified): on a keychain-SELECTED CLEAN MISS we now FALL
/// THROUGH to the file tier (was: a true "not set" that stranded file-tier
/// values — the P1-era symptom). Keychain still wins when present. Source is
/// reported truthfully (`Env`/`Keychain`/`File`/`None`) — the which-tier
/// affordance reads it.
/// (`0d0fa9e:src/secrets.ts:275-297`, extended: ADR 0010 locked-keychain
/// fallback + the B4 clean-miss fallthrough, both reported as `File`.)
pub fn resolve_secret(name: &str, env_var_name: &str, deps: &SecretDeps) -> ResolvedSecret {
    if let Some(env_val) = deps.env.var(env_var_name) {
        if !env_val.trim().is_empty() {
            return ResolvedSecret {
                value: Some(env_val),
                source: Some(Source::Env),
                locked: false,
            };
        }
    }

    if select_backend(deps) == Backend::Keychain {
        match keychain_get(name, deps) {
            KeychainOutcome::Ok(Some(v)) => {
                return ResolvedSecret {
                    value: Some(v),
                    source: Some(Source::Keychain),
                    locked: false,
                }
            }
            // B4 item 1 (read-side fallthrough): a clean keychain MISS or a
            // non-signature failure FALLS THROUGH to the file tier (was:
            // value=None, source=None — the strand). Source is `File` when the
            // file holds it, else a true `None`. Mirrors the locked-fallback
            // arm below so every store reader shares one precedence.
            KeychainOutcome::Ok(None) | KeychainOutcome::Failed(_) => {
                return resolved_file(file_get(name, deps));
            }
            KeychainOutcome::Locked => {
                // ADR 0010: a SELECTED (not env-forced) locked keychain falls to
                // the file backend so `survey` keeps working headless. Source is
                // `File` (truthful — that is where the value actually lives).
                if is_keychain_env_forced(deps) {
                    // Env-forced + locked: no fallback. The null is INACCESSIBLE,
                    // not ABSENT — flag it (`locked: true`) so callers (survey,
                    // M5) can distinguish, and emit the same one-per-process
                    // attributable diagnostic the GET path emits (orc-2 ruling
                    // relay-1780639217973-4).
                    emit_locked_diag_once(deps);
                    return ResolvedSecret {
                        value: None,
                        source: None,
                        locked: true,
                    };
                }
                emit_fallback_notice_once(deps);
                return resolved_file(file_get(name, deps));
            }
        }
    }

    resolved_file(file_get(name, deps))
}

// ----------------------------------------------------------------------------
// Masking (for `qd config get` default output) — L11.
// (`0d0fa9e:src/secrets.ts:299-303`).
// ----------------------------------------------------------------------------

/// Mask a secret for display: keep the last 4 chars, replace the rest with dots.
/// Short secrets (≤4 chars) are fully masked. NEVER prints the leading
/// characters in full. (`0d0fa9e:src/secrets.ts:301-303`.)
///
/// This is the SINGLE home for masking (A5 §3.1): `config.rs` re-exports / uses
/// it. The empty string masks to four dots (TS `value.length || 4`).
pub fn mask_secret(value: &str) -> String {
    let n = value.chars().count();
    if n <= 4 {
        // TS: "•".repeat(value.length || 4) — empty → 4 dots.
        "•".repeat(if n == 0 { 4 } else { n })
    } else {
        let last4: String = value.chars().skip(n - 4).collect();
        format!("••••{last4}")
    }
}

// ----------------------------------------------------------------------------
// Production effects (`0d0fa9e:src/secrets.ts:305-360`).
// ----------------------------------------------------------------------------

/// Probe whether the keychain backend works: `security` must be invocable. A
/// spawn error (ENOENT, no binary) => unavailable; otherwise the binary exists
/// and we treat the backend as available. Mirrors TS `realKeychainAvailable`
/// (`0d0fa9e:src/secrets.ts:321-330`) — it runs `security help` and checks that
/// the process actually ran.
pub fn real_keychain_available(exec: &dyn Exec) -> bool {
    // Ran to completion (any exit code) => the binary exists. A timeout also
    // means it ran (it was there to hang); treat as available. Spawn failure
    // (ENOENT) => not invocable.
    exec.run("security", &["help".to_string()], &[], None, None)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use crate::exec::ScriptedExec;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// An in-memory fs fake (the Rust analogue of TS `makeFileFs`,
    /// `0d0fa9e:src/secrets.test.ts:21-37`): records writes + chmods so a test
    /// can assert the [secrets] text and that chmod 600 fired.
    #[derive(Default)]
    struct FakeFs {
        files: RefCell<HashMap<String, String>>,
        chmods: RefCell<Vec<(String, u32)>>,
    }

    impl FakeFs {
        fn with(initial: &[(&str, &str)]) -> Self {
            let fs = FakeFs::default();
            for (p, t) in initial {
                fs.files.borrow_mut().insert(p.to_string(), t.to_string());
            }
            fs
        }
    }

    /// Build a [`SecretDeps`] over a `FakeFs` + `MapEnv` + `ScriptedExec`.
    /// `platform` forces the backend selection path.
    struct Harness<'a> {
        fs: &'a FakeFs,
        env: MapEnv,
        exec: &'a ScriptedExec,
        keychain_available: bool,
        platform: &'static str,
        notice: AtomicBool,
        locked_diag: AtomicBool,
    }

    impl<'a> Harness<'a> {
        /// Build a [`SecretDeps`] over this harness and pass it to `f`. The fs
        /// closures live on `run`'s stack (they cannot outlive a returned
        /// `SecretDeps`, hence the callback shape), so every test body runs
        /// inside the closure. Returns whatever `f` returns.
        fn run<R>(&self, f: impl FnOnce(&SecretDeps) -> R) -> R {
            let keychain_available = self.keychain_available;
            let avail = move || keychain_available;
            let read_file = |p: &str| self.fs.files.borrow().get(p).cloned();
            let write_file = |p: &str, t: &str| {
                self.fs
                    .files
                    .borrow_mut()
                    .insert(p.to_string(), t.to_string());
            };
            let chmod = |p: &str, m: u32| self.fs.chmods.borrow_mut().push((p.to_string(), m));
            let file_exists = |p: &str| self.fs.files.borrow().contains_key(p);
            let deps = SecretDeps {
                platform: self.platform,
                env: &self.env,
                exec: self.exec,
                keychain_available: &avail,
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

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        let mut vars = HashMap::new();
        for (k, v) in pairs {
            vars.insert(k.to_string(), v.to_string());
        }
        MapEnv { vars, uid: 501 }
    }

    // --- key registry (secrets.test.ts:84-90) ---

    #[test]
    fn openrouter_key_is_known_unknowns_rejected() {
        assert!(is_known_key("openrouter-key"));
        assert!(!is_known_key("nope"));
        assert!(known_key_names().contains(&"openrouter-key"));
        assert_eq!(
            env_var_for_key("openrouter-key"),
            Some("OPENROUTER_API_KEY")
        );
    }

    // --- backend selection (secrets.test.ts:92-114) ---

    fn select_with(platform: &'static str, env: &[(&str, &str)], avail: bool) -> Backend {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = Harness {
            fs: &fs,
            env: map_env(env),
            exec: &exec,
            keychain_available: avail,
            platform,
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        h.run(select_backend)
    }

    #[test]
    fn select_darwin_available_keychain() {
        assert_eq!(select_with("darwin", &[], true), Backend::Keychain);
        assert_eq!(select_with("macos", &[], true), Backend::Keychain);
    }
    #[test]
    fn select_darwin_unavailable_file() {
        assert_eq!(select_with("darwin", &[], false), Backend::File);
    }
    #[test]
    fn select_non_darwin_file_even_if_available() {
        assert_eq!(select_with("linux", &[], true), Backend::File);
    }
    #[test]
    fn select_env_file_forces_file_on_darwin() {
        assert_eq!(
            select_with("darwin", &[("QD_SECRET_BACKEND", "file")], true),
            Backend::File
        );
    }
    #[test]
    fn select_env_keychain_forces_keychain_on_linux() {
        assert_eq!(
            select_with("linux", &[("QD_SECRET_BACKEND", "keychain")], false),
            Backend::Keychain
        );
    }
    #[test]
    fn select_invalid_env_value_silently_ignored() {
        // PORT NOTE: TS silently falls through on an unknown value (no notice).
        assert_eq!(
            select_with("linux", &[("QD_SECRET_BACKEND", "bogus")], true),
            Backend::File
        );
        assert_eq!(
            select_with("darwin", &[("QD_SECRET_BACKEND", "bogus")], true),
            Backend::Keychain
        );
    }

    // --- config path (secrets.test.ts:116-124) ---

    #[test]
    fn config_path_honors_qd_home() {
        let env = map_env(&[("QD_HOME", "/tmp/qb-test")]);
        assert_eq!(resolve_config_path(&env), "/tmp/qb-test/config.toml");
    }
    #[test]
    fn config_path_defaults_to_dot_qd_under_home() {
        let env = map_env(&[("HOME", "/home/x")]);
        assert_eq!(
            resolve_config_path(&env),
            "/home/x/.quorum/dispatch/config.toml"
        );
    }

    // --- TOML round-trip (secrets.test.ts:126-143) ---

    #[test]
    fn toml_round_trip_identity() {
        let table = vec![
            ("openrouter-key".to_string(), "sk-or-abcd1234".to_string()),
            ("other-key".to_string(), "v".to_string()),
        ];
        let text = serialize_secrets_toml(&table);
        assert!(text.contains("[secrets]"));
        let parsed = parse_secrets_toml(&text);
        // sorted comparison (serialize sorts)
        let mut want = table.clone();
        want.sort();
        let mut got = parsed.clone();
        got.sort();
        assert_eq!(got, want);
    }
    #[test]
    fn toml_handles_quotes_and_backslashes() {
        let table = vec![("openrouter-key".to_string(), "a\"b\\c".to_string())];
        let parsed = parse_secrets_toml(&serialize_secrets_toml(&table));
        assert_eq!(parsed, table);
    }
    #[test]
    fn toml_ignores_non_secrets_sections_and_comments() {
        let text = "# hi\n[other]\nx = \"1\"\n[secrets]\nopenrouter-key = \"sk\"\n";
        assert_eq!(
            parse_secrets_toml(text),
            vec![("openrouter-key".to_string(), "sk".to_string())]
        );
    }
    #[test]
    fn toml_permissive_read_of_dirty_config() {
        // G-C2: junk lines, malformed kv, an unterminated quote — permissive
        // read keeps the well-formed pair, drops the rest (L8).
        let text = "[secrets]\ngarbage line no equals\nopenrouter-key = \"good\"\nbad = \"unterminated\nnumber = 42\n";
        assert_eq!(
            parse_secrets_toml(text),
            vec![("openrouter-key".to_string(), "good".to_string())]
        );
    }

    // --- file backend (secrets.test.ts:145-205) ---

    fn file_harness<'a>(
        fs: &'a FakeFs,
        exec: &'a ScriptedExec,
        env: &[(&str, &str)],
    ) -> Harness<'a> {
        Harness {
            fs,
            env: map_env(env),
            exec,
            keychain_available: false,
            platform: "linux",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        }
    }

    #[test]
    fn file_set_writes_toml_and_chmods_600_get_round_trips() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        h.run(|deps| {
            assert_eq!(
                set_secret("openrouter-key", "sk-or-FAKE-secret9999", deps),
                Ok(Backend::File)
            );
            let path = "/quorum/qd/config.toml";
            assert!(fs.files.borrow()[path].contains("openrouter-key = \"sk-or-FAKE-secret9999\""));
            assert!(fs
                .chmods
                .borrow()
                .iter()
                .any(|(p, m)| p == path && *m == 0o600));
            assert_eq!(
                get_secret("openrouter-key", deps),
                Some("sk-or-FAKE-secret9999".to_string())
            );
        });
    }

    #[test]
    fn file_get_returns_none_when_absent() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        h.run(|deps| assert_eq!(get_secret("openrouter-key", deps), None));
    }

    #[test]
    fn file_set_chmods_600_even_when_updating_existing_file() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        h.run(|deps| {
            set_secret("openrouter-key", "first", deps).unwrap();
            fs.chmods.borrow_mut().clear();
            set_secret("openrouter-key", "second", deps).unwrap();
            assert!(fs.chmods.borrow().iter().any(|(_, m)| *m == 0o600));
            assert_eq!(
                get_secret("openrouter-key", deps),
                Some("second".to_string())
            );
        });
    }

    #[test]
    fn file_delete_removes_the_key() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        h.run(|deps| {
            set_secret("openrouter-key", "v", deps).unwrap();
            delete_secret("openrouter-key", deps);
            assert_eq!(get_secret("openrouter-key", deps), None);
        });
    }

    #[test]
    fn file_backend_info_reports_file_path_keys_never_values() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        h.run(|deps| {
            set_secret("openrouter-key", "sekret", deps).unwrap();
            let info = secret_backend_info(deps);
            assert_eq!(info.backend, Backend::File);
            assert_eq!(info.file_path, "/quorum/qd/config.toml");
            assert_eq!(
                info.keys_set,
                vec![("openrouter-key".to_string(), Source::File)]
            );
            assert!(!format!("{info:?}").contains("sekret"));
        });
    }

    // --- keychain backend (secrets.test.ts:207-263) ---

    /// Build a keychain-backend harness over a caller-owned scratch fs. The
    /// keychain path never touches the fs except under fallback (which the
    /// fallback rows below assert explicitly), so a fresh empty FakeFs is safe.
    fn keychain_harness<'a>(
        fs: &'a FakeFs,
        exec: &'a ScriptedExec,
        env: &[(&str, &str)],
    ) -> Harness<'a> {
        Harness {
            fs,
            env: map_env(env),
            exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        }
    }

    #[test]
    fn keychain_set_constructs_right_argv() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on("security", &["add-generic-password"], Some(0), "", "");
        let h = keychain_harness(&fs, &exec, &[]);
        h.run(|deps| {
            assert_eq!(
                set_secret("openrouter-key", "sk-or-FAKE-zzz", deps),
                Ok(Backend::Keychain)
            );
        });
        let log = exec.log();
        assert_eq!(log[0].cmd, "security");
        assert_eq!(
            log[0].args,
            keychain_set_argv("openrouter-key", "sk-or-FAKE-zzz")
        );
        assert_eq!(
            log[0].args,
            vec![
                "add-generic-password",
                "-U",
                "-a",
                "openrouter-key",
                "-s",
                "qd-cli",
                "-w",
                "sk-or-FAKE-zzz"
            ]
        );
    }

    #[test]
    fn keychain_get_parses_found_value_strips_newline() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "sk-or-FAKE-found\n",
            "",
        );
        let h = keychain_harness(&fs, &exec, &[]);
        h.run(|deps| {
            assert_eq!(
                get_secret("openrouter-key", deps),
                Some("sk-or-FAKE-found".to_string())
            );
        });
        assert_eq!(exec.log()[0].args, keychain_get_argv("openrouter-key"));
    }

    #[test]
    fn keychain_get_none_on_nonzero_exit() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(44),
            "",
            "not found",
        );
        let h = keychain_harness(&fs, &exec, &[]);
        h.run(|deps| assert_eq!(get_secret("openrouter-key", deps), None));
    }

    #[test]
    fn keychain_get_none_on_empty_stdout_exit_0() {
        let fs = FakeFs::default();
        let exec =
            ScriptedExec::new().on("security", &["find-generic-password"], Some(0), "\n", "");
        let h = keychain_harness(&fs, &exec, &[]);
        h.run(|deps| assert_eq!(get_secret("openrouter-key", deps), None));
    }

    #[test]
    fn keychain_delete_constructs_right_argv() {
        let fs = FakeFs::default();
        let exec =
            ScriptedExec::new().on("security", &["delete-generic-password"], Some(0), "", "");
        let h = keychain_harness(&fs, &exec, &[]);
        h.run(|deps| delete_secret("openrouter-key", deps));
        assert_eq!(exec.log()[0].args, keychain_delete_argv("openrouter-key"));
    }

    #[test]
    fn keychain_set_errors_on_nonzero_nonsignature_exit() {
        let fs = FakeFs::default();
        let exec =
            ScriptedExec::new().on("security", &["add-generic-password"], Some(1), "", "boom");
        let h = keychain_harness(&fs, &exec, &[]);
        let r = h.run(|deps| set_secret("openrouter-key", "v", deps));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("keychain write failed"));
    }

    #[test]
    fn keychain_backend_info_reports_only_set_known_keys() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "present\n",
            "",
        );
        let h = keychain_harness(&fs, &exec, &[]);
        let info = h.run(secret_backend_info);
        assert_eq!(info.backend, Backend::Keychain);
        assert_eq!(
            info.keys_set,
            vec![("openrouter-key".to_string(), Source::Keychain)]
        );
        assert!(!format!("{info:?}").contains("present"));
    }

    // --- resolveSecret precedence (secrets.test.ts:265-313) ---

    #[test]
    fn resolve_env_wins_over_keychain() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "sk-keychain\n",
            "",
        );
        let h = keychain_harness(&fs, &exec, &[("OPENROUTER_API_KEY", "sk-env")]);
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-env".to_string()));
        assert_eq!(r.source, Some(Source::Env));
    }

    #[test]
    fn resolve_env_wins_over_file() {
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-file\"\n",
        )]);
        let exec = ScriptedExec::new();
        let h = file_harness(
            &fs,
            &exec,
            &[("QD_HOME", "/quorum/qd"), ("OPENROUTER_API_KEY", "sk-env")],
        );
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-env".to_string()));
        assert_eq!(r.source, Some(Source::Env));
    }

    #[test]
    fn resolve_keychain_when_env_empty_keychain_active() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "sk-keychain\n",
            "",
        );
        let h = keychain_harness(&fs, &exec, &[]);
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-keychain".to_string()));
        assert_eq!(r.source, Some(Source::Keychain));
    }

    #[test]
    fn resolve_file_when_env_empty_keychain_unavailable() {
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-file\"\n",
        )]);
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-file".to_string()));
        assert_eq!(r.source, Some(Source::File));
    }

    #[test]
    fn resolve_nothing_anywhere_null() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, None);
        assert_eq!(r.source, None);
    }

    // --- masking (secrets.test.ts:315-324) ---

    #[test]
    fn mask_keeps_last_4() {
        assert_eq!(mask_secret("sk-or-v1-abcd1234"), "••••1234");
    }
    #[test]
    fn mask_short_fully_masked() {
        assert_eq!(mask_secret("ab"), "••");
        assert_eq!(mask_secret(""), "••••");
    }

    // ========================================================================
    // NEW: ADR 0010 locked-keychain fallback rows (A5 §3.2 / §3.4).
    // ========================================================================

    /// A `security` shim that emits the locked signature on stderr + exit 36
    /// (mirroring the inbox live capture) for SET, and succeeds for the GET that
    /// `set_secret` does not perform. The file backend is a real FakeFs here so
    /// we can assert the fallback WRITE + chmod 600 landed.
    #[test]
    fn fallback_selected_keychain_locked_set_writes_file_and_chmods_600() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["add-generic-password"],
            Some(36),
            "",
            "security: SecKeychainItemCreateFromContent (<default>): User interaction is not allowed.",
        );
        // keychain SELECTED (darwin + available), NOT env-forced.
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        // set falls back to file → reports Backend::File (truthful storing backend).
        h.run(|deps| {
            assert_eq!(
                set_secret("openrouter-key", "sk-or-FAKE-fallback", deps),
                Ok(Backend::File)
            );
        });
        let path = "/quorum/qd/config.toml";
        assert!(fs.files.borrow()[path].contains("openrouter-key = \"sk-or-FAKE-fallback\""));
        // chmod 600 asserted after the FALLBACK write too.
        assert!(fs
            .chmods
            .borrow()
            .iter()
            .any(|(p, m)| p == path && *m == 0o600));
    }

    #[test]
    fn fallback_env_forced_keychain_locked_set_errors_loud_no_file_write() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["add-generic-password"],
            Some(36),
            "",
            "security: User interaction is not allowed.",
        );
        // env-forced keychain (even on linux): NEVER falls back.
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd"), ("QD_SECRET_BACKEND", "keychain")]),
            exec: &exec,
            keychain_available: false,
            platform: "linux",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let r = h.run(|deps| set_secret("openrouter-key", "sk-or-FAKE-x", deps));
        assert!(r.is_err(), "env-forced keychain must fail loud");
        assert!(r.unwrap_err().contains("forbids file fallback"));
        // NO file write happened.
        assert!(fs.files.borrow().is_empty());
        assert!(fs.chmods.borrow().is_empty());
    }

    #[test]
    fn fallback_non_signature_failure_no_fallback_keeps_ts_semantics() {
        // A non-signature keychain failure (exit 1, generic stderr) must NOT
        // fall back — it surfaces loud, file untouched.
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["add-generic-password"],
            Some(1),
            "",
            "security: some other error",
        );
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let r = h.run(|deps| set_secret("openrouter-key", "v", deps));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("keychain write failed"));
        assert!(
            fs.files.borrow().is_empty(),
            "no fallback write on non-signature failure"
        );
    }

    #[test]
    fn fallback_backend_info_keys_set_enumerates_via_file_under_lock() {
        // R5: backend_info per-key gets hit the lock signature and fall back to
        // a file read → keys_set truthfully lists the fallback-file keys, while
        // the SELECTION stays keychain.
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-or-FAKE-infile\"\n",
        )]);
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(36),
            "",
            "security: User interaction is not allowed.",
        );
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let info = h.run(secret_backend_info);
        // Backend line = SELECTION (keychain).
        assert_eq!(info.backend, Backend::Keychain);
        // Keys set = EFFECTIVE (read from the fallback file) — tier=File.
        assert_eq!(
            info.keys_set,
            vec![("openrouter-key".to_string(), Source::File)]
        );
    }

    #[test]
    fn fallback_resolve_secret_under_lock_reads_file_source_file() {
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-or-FAKE-resolved\"\n",
        )]);
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(36),
            "",
            "User interaction is not allowed",
        );
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-or-FAKE-resolved".to_string()));
        // Source is File — the value truly lives in the fallback file.
        assert_eq!(r.source, Some(Source::File));
    }

    // --- MUST-FAIL negative control (G-N1 teeth) ---

    #[test]
    fn negative_control_wrong_signature_string_does_not_fall_back() {
        // TEETH: a keychain failure carrying a DIFFERENT stderr string (NOT the
        // exact signature) must NOT trigger fallback. If detection were
        // loosened to match any "interaction" substring or keyed on exit 36,
        // this would wrongly fall back to file and the assertion below (err +
        // empty file) would RED. This is the mutation guard for the
        // string-only detection contract.
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["add-generic-password"],
            Some(36), // SAME exit code as the live lock — proves exit is NOT the key
            "",
            "security: user interaction was not permitted", // paraphrase, NOT the signature
        );
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let r = h.run(|deps| set_secret("openrouter-key", "v", deps));
        // Must be a LOUD error (no fallback), file untouched.
        assert!(
            r.is_err(),
            "paraphrased non-signature string must NOT fall back"
        );
        assert!(
            fs.files.borrow().is_empty(),
            "no fallback write may happen on a non-signature failure"
        );
    }

    // ========================================================================
    // NEW: env-forced-locked GET diagnostic (orc-2 ruling
    // relay-1780639217973-4 "middle path c"; A5 §3.2).
    //
    // The diagnostic is a single stderr line gated by the once-per-process
    // `locked_diag` flag (the sole `eprintln!` site). These rows assert the
    // FLAG transitions (false→true exactly when the env-forced-locked signature
    // is hit, never otherwise) — the same structural pattern the fallback rows
    // use for `fallback_notice_emitted`, since the crate has no stderr-capture
    // seam. The verbatim line text is pinned by `diag_line_is_verbatim`.
    // ========================================================================

    /// Build a `security` GET shim that emits the locked signature + exit 36.
    fn locked_get_exec() -> ScriptedExec {
        ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(36),
            "",
            "security: SecKeychainItemCreateFromContent (<default>): User interaction is not allowed.",
        )
    }

    #[test]
    fn diag_line_is_verbatim() {
        // Pins the EXACT ruling text so a reword reds here (the line is an
        // operator-facing contract from orc-2 relay-1780639217973-4).
        assert_eq!(
            ENV_FORCED_LOCKED_DIAG,
            "warning: keychain is locked — a key may exist but is inaccessible (QD_SECRET_BACKEND=keychain is env-forced; unlock or unset to use fallback)."
        );
    }

    #[test]
    fn env_forced_locked_get_returns_none_and_emits_diag_once_across_two_gets() {
        // TS-PARITY: env-forced + locked GET returns None (the caller maps that
        // to `<key>: not set.` exit 0 — stdout/exit UNCHANGED). DIVERGENCE: the
        // once-per-process diagnostic fires. Two gets => the flag flips exactly
        // once (the sole eprintln! is gated on the false→true swap), so the
        // operator sees ONE line, not one per probe.
        let fs = FakeFs::default();
        let exec = locked_get_exec();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd"), ("QD_SECRET_BACKEND", "keychain")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        h.run(|deps| {
            // TS-parity stdout/exit contract: None (=> not-set line, exit 0).
            assert_eq!(get_secret("openrouter-key", deps), None);
            // The diagnostic fired (flag set) — attributable-locked path taken.
            assert!(
                deps.locked_diag_emitted.load(Ordering::SeqCst),
                "first env-forced-locked get must emit the diagnostic"
            );
            // Second get: still None, flag stays set → the single eprintln! site
            // is already consumed, so NO second line is emitted (once-per-proc).
            assert_eq!(get_secret("openrouter-key", deps), None);
            assert!(deps.locked_diag_emitted.load(Ordering::SeqCst));
        });
        // The fallback NOTICE must NOT have fired — env-forced never falls back.
        assert!(
            !h.notice.load(Ordering::SeqCst),
            "env-forced lock must not emit the file-fallback notice"
        );
    }

    #[test]
    fn selected_locked_get_emits_fallback_notice_not_the_env_forced_diag() {
        // A SELECTED (not env-forced) locked keychain falls back to file: the
        // FALLBACK notice fires, the env-forced diagnostic does NOT.
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-or-FAKE-sel\"\n",
        )]);
        let exec = locked_get_exec();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]), // NOT env-forced
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let got = h.run(|deps| get_secret("openrouter-key", deps));
        assert_eq!(
            got,
            Some("sk-or-FAKE-sel".to_string()),
            "falls back to file"
        );
        assert!(
            h.notice.load(Ordering::SeqCst),
            "selected-lock get must emit the fallback notice"
        );
        assert!(
            !h.locked_diag.load(Ordering::SeqCst),
            "selected-lock get must NOT emit the env-forced diagnostic"
        );
    }

    #[test]
    fn plain_not_found_get_emits_neither_diagnostic() {
        // NEGATIVE CONTROL: an ordinary not-found (non-zero exit, NOT the locked
        // signature) under env-forced keychain returns None and emits NEITHER
        // line. Guards against the diagnostic leaking onto the plain not-set
        // path (which would break presence-probing operators with noise).
        let fs = FakeFs::default();
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(44),
            "",
            "security: SecKeychainSearchCopyNext: The specified item could not be found.",
        );
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd"), ("QD_SECRET_BACKEND", "keychain")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let got = h.run(|deps| get_secret("openrouter-key", deps));
        assert_eq!(got, None);
        assert!(
            !h.locked_diag.load(Ordering::SeqCst),
            "plain not-found must NOT emit the env-forced diagnostic"
        );
        assert!(
            !h.notice.load(Ordering::SeqCst),
            "plain not-found must NOT emit the fallback notice"
        );
    }

    #[test]
    fn resolve_secret_sets_locked_flag_only_on_env_forced_locked_arm() {
        // The richer resolve API: `locked` distinguishes INACCESSIBLE from
        // ABSENT. It is true ONLY on the env-forced + locked arm.
        let fs = FakeFs::default();
        let exec = locked_get_exec();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd"), ("QD_SECRET_BACKEND", "keychain")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, None);
        assert_eq!(r.source, None);
        assert!(
            r.locked,
            "env-forced + locked => locked flag set (inaccessible)"
        );
        assert!(
            h.locked_diag.load(Ordering::SeqCst),
            "resolve env-forced-locked arm also emits the diagnostic"
        );
    }

    #[test]
    fn resolve_secret_locked_flag_false_on_absent_and_fallback_and_env_paths() {
        // Plain absent (no key anywhere) → locked=false (ABSENT, not locked).
        {
            let fs = FakeFs::default();
            let exec = ScriptedExec::new();
            let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
            let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
            assert_eq!(r.value, None);
            assert!(!r.locked, "plain-absent must not set locked");
        }
        // SELECTED (not env-forced) locked → falls back to file, locked=false
        // (the value is accessible via the file, so not inaccessible).
        {
            let fs = FakeFs::with(&[(
                "/quorum/qd/config.toml",
                "[secrets]\nopenrouter-key = \"sk-or-FAKE-fb\"\n",
            )]);
            let exec = locked_get_exec();
            let h = Harness {
                fs: &fs,
                env: map_env(&[("QD_HOME", "/quorum/qd")]),
                exec: &exec,
                keychain_available: true,
                platform: "darwin",
                notice: AtomicBool::new(false),
                locked_diag: AtomicBool::new(false),
            };
            let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
            assert_eq!(r.value, Some("sk-or-FAKE-fb".to_string()));
            assert!(
                !r.locked,
                "selected-lock fallback value is accessible => not locked"
            );
        }
        // Env wins → locked=false.
        {
            let fs = FakeFs::default();
            let exec = ScriptedExec::new();
            let h = keychain_harness(&fs, &exec, &[("OPENROUTER_API_KEY", "sk-env")]);
            let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
            assert_eq!(r.source, Some(Source::Env));
            assert!(!r.locked, "env-sourced value is not locked");
        }
    }

    // --- Plain file-tier keys (punch item 7: render-default) ----------------

    #[test]
    fn render_default_is_known_plain_no_env_override() {
        assert!(is_known_key("render-default"));
        assert!(is_plain_file_key("render-default"));
        assert!(!is_plain_file_key("openrouter-key"));
        assert!(known_key_names().contains(&"render-default"));
        // Plain keys have NO env override.
        assert_eq!(env_var_for_key("render-default"), None);
    }

    /// Teaching error at SET time (punch item 7): only inline | alt-screen are
    /// legal; anything else is rejected with a message that names both values.
    #[test]
    fn validate_render_default_values() {
        assert_eq!(validate_key_value("render-default", "inline"), None);
        assert_eq!(validate_key_value("render-default", "alt-screen"), None);
        let err = validate_key_value("render-default", "fullscreen").expect("rejected");
        assert!(err.contains("invalid value 'fullscreen'"));
        assert!(err.contains("'inline'"));
        assert!(err.contains("'alt-screen'"));
        // Secret keys are not value-validated.
        assert_eq!(validate_key_value("openrouter-key", "anything"), None);
    }

    /// render-default lives in the FILE tier even when the keychain is the
    /// selected backend (darwin + available): set routes to the file (reported
    /// backend = file), get reads the file, and `security` is NEVER invoked.
    #[test]
    fn plain_key_bypasses_keychain_entirely() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new(); // NO canned `security` responses.
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: true,
            platform: "darwin",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let backend = h
            .run(|deps| set_secret("render-default", "alt-screen", deps))
            .unwrap();
        assert_eq!(backend, Backend::File, "plain keys store in the FILE tier");
        assert_eq!(
            h.run(|deps| get_secret("render-default", deps)),
            Some("alt-screen".to_string())
        );
        // The keychain was never consulted for set OR get.
        assert!(
            exec.log().is_empty(),
            "no `security` invocation for a plain key: {:?}",
            exec.log()
        );
        // The file write kept 0600 (it may also hold secrets).
        assert!(fs
            .chmods
            .borrow()
            .iter()
            .any(|(p, m)| p == "/quorum/qd/config.toml" && *m == 0o600));
        // Stored as a TOP-LEVEL line, not in [secrets].
        let text = fs.files.borrow().get("/quorum/qd/config.toml").cloned().unwrap();
        assert_eq!(text, "render-default = \"alt-screen\"\n");
    }

    /// The plain-key writer PRESERVES every other byte of the file: comments,
    /// the top-level claude_flags line, and the [secrets] table all survive a
    /// set / update / unset cycle untouched.
    #[test]
    fn plain_key_upsert_preserves_other_content() {
        let initial =
            "# hand comment\nclaude_flags = \"--a --b\"\n[secrets]\nopenrouter-key = \"sk-x\"\n";
        let fs = FakeFs::with(&[("/quorum/qd/config.toml", initial)]);
        let exec = ScriptedExec::new();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: false,
            platform: "linux",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        // Insert: lands at the end of the top-level region, BEFORE [secrets].
        h.run(|deps| set_secret("render-default", "inline", deps))
            .unwrap();
        let text = fs.files.borrow().get("/quorum/qd/config.toml").cloned().unwrap();
        assert_eq!(
            text,
            "# hand comment\nclaude_flags = \"--a --b\"\nrender-default = \"inline\"\n[secrets]\nopenrouter-key = \"sk-x\"\n"
        );
        // Update: replaced in place.
        h.run(|deps| set_secret("render-default", "alt-screen", deps))
            .unwrap();
        let text = fs.files.borrow().get("/quorum/qd/config.toml").cloned().unwrap();
        assert!(text.contains("render-default = \"alt-screen\""));
        assert!(!text.contains("render-default = \"inline\""));
        // The secrets table reads back unchanged through the secrets path.
        assert_eq!(
            h.run(|deps| get_secret("openrouter-key", deps)),
            Some("sk-x".to_string())
        );
        // Unset: back to the original bytes.
        h.run(|deps| delete_secret("render-default", deps));
        let text = fs.files.borrow().get("/quorum/qd/config.toml").cloned().unwrap();
        assert_eq!(text, initial);
    }

    /// A `render-default` line INSIDE the [secrets] section is NOT a top-level
    /// key (the reader stops at the first header), and the file-branch
    /// backend_info enumerates a set plain key.
    #[test]
    fn plain_key_reader_is_top_level_only_and_info_lists_it() {
        assert_eq!(
            get_plain_config_key("[secrets]\nrender-default = \"inline\"\n", "render-default"),
            None
        );
        assert_eq!(
            get_plain_config_key("render-default = \"inline\"\n[secrets]\n", "render-default"),
            Some("inline".to_string())
        );
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "render-default = \"inline\"\n[secrets]\nopenrouter-key = \"sk-x\"\n",
        )]);
        let exec = ScriptedExec::new();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: false,
            platform: "linux",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        let info = h.run(secret_backend_info);
        assert_eq!(
            info.keys_set,
            vec![
                ("openrouter-key".to_string(), Source::File),
                ("render-default".to_string(), Source::File)
            ]
        );
    }

    /// punch item 7 (clobber guard): a FILE-BACKEND secret write must PRESERVE
    /// non-[secrets] content — top-level plain keys (render-default,
    /// claude_flags), hand comments, and other sections. Previously the write
    /// replaced the whole file with header + [secrets], so setting a secret on
    /// the file tier silently ERASED render-default.
    #[test]
    fn secret_write_preserves_top_level_keys_and_other_sections() {
        let initial = "# hand comment\nclaude_flags = \"--a\"\nrender-default = \"alt-screen\"\n\
                       [secrets]\nopenrouter-key = \"sk-old\"\n[other]\nx = \"1\"\n";
        let fs = FakeFs::with(&[("/quorum/qd/config.toml", initial)]);
        let exec = ScriptedExec::new();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: false,
            platform: "linux",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        h.run(|deps| set_secret("openrouter-key", "sk-new", deps))
            .unwrap();
        let text = fs.files.borrow().get("/quorum/qd/config.toml").cloned().unwrap();
        assert_eq!(
            text,
            "# qd config -- managed by `qd config`. Do not hand-edit secret values.\n\
             # hand comment\nclaude_flags = \"--a\"\nrender-default = \"alt-screen\"\n\
             [secrets]\nopenrouter-key = \"sk-new\"\n[other]\nx = \"1\"\n"
        );
        // And the plain key still reads back through both surfaces.
        assert_eq!(
            h.run(|deps| get_secret("render-default", deps)),
            Some("alt-screen".to_string())
        );
    }

    /// With NO preserved content the merged write is byte-identical to
    /// `serialize_secrets_toml` (the TS-parity file shape stands).
    #[test]
    fn secret_write_without_extra_content_keeps_ts_parity_shape() {
        let fs = FakeFs::default();
        let exec = ScriptedExec::new();
        let h = Harness {
            fs: &fs,
            env: map_env(&[("QD_HOME", "/quorum/qd")]),
            exec: &exec,
            keychain_available: false,
            platform: "linux",
            notice: AtomicBool::new(false),
            locked_diag: AtomicBool::new(false),
        };
        h.run(|deps| set_secret("openrouter-key", "sk-x", deps))
            .unwrap();
        let text = fs.files.borrow().get("/quorum/qd/config.toml").cloned().unwrap();
        assert_eq!(
            text,
            serialize_secrets_toml(&[("openrouter-key".to_string(), "sk-x".to_string())])
        );
    }

    /// upsert_top_level_key edge shapes: empty file, file with no sections,
    /// remove-missing is a no-op.
    #[test]
    fn upsert_top_level_key_edges() {
        assert_eq!(
            upsert_top_level_key("", "render-default", Some("inline")),
            "render-default = \"inline\"\n"
        );
        assert_eq!(
            upsert_top_level_key("a = \"1\"\n", "render-default", Some("inline")),
            "a = \"1\"\nrender-default = \"inline\"\n"
        );
        assert_eq!(
            upsert_top_level_key("a = \"1\"\n", "render-default", None),
            "a = \"1\"\n"
        );
        assert_eq!(upsert_top_level_key("", "render-default", None), "");
    }

    // ========================================================================
    // qb punch B4 item 1 — TIER-STRANDING fix (Phase-1 repro → Phase-2 fix).
    //
    // PHASE-1 verdict (orc-ratified): the divergence reproduced as
    // TIER-STRANDING. All store readers share one precedence, but a
    // keychain-SELECTED clean miss returned "not set" WITHOUT consulting the
    // file tier, while writers legitimately land file-tier values:
    //   (w1) ADR-0010 locked-keychain set fallback (headless/SSH),
    //   (w2) `QD_SECRET_BACKEND=file qd config set …` — the engine's OWN
    //        non-TTY teaching error recommends exactly this form,
    //   (w3) a hand-edit / restore of ~/.quorum/dispatch/config.toml.
    //
    // PHASE-2 fix (this commit): `get_secret` + `resolve_secret` now FALL
    // THROUGH to the file tier on a clean keychain miss (keychain still wins
    // when present). The former DEFECT rows below are FLIPPED to HELD — they
    // now assert the value round-trips across the lock-state change, for every
    // store consumer at once (`qd config get`, `qd survey`, `qd start --via`,
    // `qd config path`). Read-side only: `set_secret`/`delete_secret` are
    // unchanged (no set-side cross-tier self-heal — orc ruling).
    // ========================================================================

    /// `security` shim: ADD (set) hits the locked signature; FIND (get) is an
    /// unlocked clean miss (exit 44, no signature). Used to model the two
    /// process lifetimes of the w1 timeline against ONE persistent FakeFs.
    fn locked_add_exec() -> ScriptedExec {
        ScriptedExec::new().on(
            "security",
            &["add-generic-password"],
            Some(36),
            "",
            "security: SecKeychainItemCreateFromContent (<default>): User interaction is not allowed.",
        )
    }
    fn unlocked_empty_find_exec() -> ScriptedExec {
        ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(44),
            "",
            "security: The specified item could not be found in the keychain.",
        )
    }

    /// HELD (was DEFECT — the w1 timeline, now FIXED): process 1 runs
    /// `qd config set` under a LOCKED keychain → ADR-0010 fallback stores the
    /// value in the file tier. Process 2 — same config root, keychain now
    /// UNLOCKED but holding no item — runs every store-reader's resolution:
    /// ALL now find the file-tier key via the clean-miss fallthrough. `qd
    /// config get` reports it (tier=file); `qd survey` resolves it (source
    /// File); `qd config path` lists it (file). The strand is closed.
    #[test]
    fn held_item1_locked_set_then_unlocked_read_resolves_file_tier() {
        let fs = FakeFs::default(); // the persistent config root across both "processes"
        let key = "openrouter-key";

        // Process 1: headless set; keychain selected (darwin+available), locked.
        let set_exec = locked_add_exec();
        let h_set = keychain_harness(&fs, &set_exec, &[("QD_HOME", "/quorum/qd")]);
        let stored = h_set.run(|deps| set_secret(key, "sk-or-FAKE-stranded", deps));
        assert_eq!(
            stored,
            Ok(Backend::File),
            "ADR-0010 fallback stored to file"
        );
        assert!(
            fs.files.borrow()["/quorum/qd/config.toml"].contains("sk-or-FAKE-stranded"),
            "the value persists in the file tier"
        );

        // Process 2: same root, keychain unlocked + empty (clean miss).
        let get_exec = unlocked_empty_find_exec();
        let h_get = keychain_harness(&fs, &get_exec, &[("QD_HOME", "/quorum/qd")]);

        // `qd config get` reader (config.rs RealStore::get → get_secret).
        assert_eq!(
            h_get.run(|deps| get_secret(key, deps)),
            Some("sk-or-FAKE-stranded".to_string()),
            "FIXED: get_secret falls through to the file-tier value on a clean miss"
        );
        // `qd survey` reader (bin/dispatch/survey.rs resolve_api_key → resolve_secret).
        let r = h_get.run(|deps| resolve_secret(key, "OPENROUTER_API_KEY", deps));
        assert_eq!(
            (r.value, r.source, r.locked),
            (
                Some("sk-or-FAKE-stranded".to_string()),
                Some(Source::File),
                false
            ),
            "FIXED: resolve_secret reports the file-tier value (source File)"
        );
        // `qd start --via` secret-credential reader uses the same get_secret
        // path — fixed by the get_secret assertion above.

        // `qd config path` Keys-set enumeration: the key is listed with tier=File.
        let info = h_get.run(secret_backend_info);
        assert_eq!(info.backend, Backend::Keychain); // SELECTION unchanged
        assert_eq!(
            info.keys_set,
            vec![("openrouter-key".to_string(), Source::File)],
            "FIXED: `qd config path` lists the file-stranded key with tier=File"
        );
    }

    /// HELD (was DEFECT — w2, now FIXED): `QD_SECRET_BACKEND=file qd config set
    /// …` (the exact form config.rs's non-TTY error recommends) stores to the
    /// file tier; a later read WITHOUT that env var (default selection:
    /// keychain) now FINDS it via the clean-miss fallthrough.
    #[test]
    fn held_item1_env_forced_file_set_then_default_read_resolves_file_tier() {
        let fs = FakeFs::default();
        let key = "openrouter-key";

        // Set: QD_SECRET_BACKEND=file (per the engine's own non-TTY hint).
        let set_exec = ScriptedExec::new();
        let h_set = keychain_harness(
            &fs,
            &set_exec,
            &[("QD_HOME", "/quorum/qd"), ("QD_SECRET_BACKEND", "file")],
        );
        assert_eq!(
            h_set.run(|deps| set_secret(key, "sk-or-FAKE-filetier", deps)),
            Ok(Backend::File)
        );
        // No keychain write was ever attempted (keychain-safety sanity).
        assert!(set_exec.log().is_empty());

        // Read: default env (no QD_SECRET_BACKEND) → keychain selected, clean miss.
        let get_exec = unlocked_empty_find_exec();
        let h_get = keychain_harness(&fs, &get_exec, &[("QD_HOME", "/quorum/qd")]);
        assert_eq!(
            h_get.run(|deps| get_secret(key, deps)),
            Some("sk-or-FAKE-filetier".to_string()),
            "FIXED: the value the engine's own hint stored is now found via the \
             clean-miss fallthrough on a default-selection read"
        );
        let r = h_get.run(|deps| resolve_secret(key, "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-or-FAKE-filetier".to_string()));
        assert_eq!(
            r.source,
            Some(Source::File),
            "FIXED: survey's reader finds it too"
        );
    }

    /// AFFORDANCE PIN (the Phase-1 asymmetry, now CLOSED): with the env var
    /// exported and the store empty, BOTH surfaces agree the value resolves
    /// from the ENV tier — `qd survey`'s reader (`resolve_secret`) AND `qd
    /// config get`'s reader (`resolve_config_tier`, which `config get` now
    /// uses). Phase-1 pinned the divergence (the old `get_secret` was
    /// store-only → "not set" while survey saw the env value); the affordance
    /// resolution closes it. (`qd start --via` retains NO env tier BY PINNED
    /// DESIGN — profile determinism, red-team F3/F10 — that asymmetry is
    /// intentional and not a divergence.)
    #[test]
    fn affordance_item1_env_tier_agrees_config_get_and_survey() {
        let fs = FakeFs::default();
        let exec = unlocked_empty_find_exec();
        let h = keychain_harness(
            &fs,
            &exec,
            &[
                ("QD_HOME", "/quorum/qd"),
                ("OPENROUTER_API_KEY", "sk-or-FAKE-env"),
            ],
        );
        // survey's reader: env wins.
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-or-FAKE-env".to_string()));
        assert_eq!(r.source, Some(Source::Env));
        // config get's reader (resolve_config_tier): ALSO env now — asymmetry closed.
        let c = h.run(|deps| resolve_config_tier("openrouter-key", deps));
        assert_eq!(c.value, Some("sk-or-FAKE-env".to_string()));
        assert_eq!(c.source, Some(Source::Env));
        // The low-level get_secret remains store-only BY DESIGN (no env tier);
        // config get no longer routes through it directly.
        assert_eq!(h.run(|deps| get_secret("openrouter-key", deps)), None);
    }

    /// HELD (falsification row): in a PERSISTENTLY locked environment (locked
    /// at set time AND read time — the steady-state headless host) the w1
    /// timeline is coherent: set falls back to the file, read falls back to
    /// the file, the value round-trips. The strand needs the lock state (or
    /// the backend selection) to CHANGE between write and read.
    #[test]
    fn held_item1_persistently_locked_environment_round_trips() {
        let fs = FakeFs::default();
        let key = "openrouter-key";

        let set_exec = locked_add_exec();
        let h_set = keychain_harness(&fs, &set_exec, &[("QD_HOME", "/quorum/qd")]);
        assert_eq!(
            h_set.run(|deps| set_secret(key, "sk-or-FAKE-headless", deps)),
            Ok(Backend::File)
        );

        // Read under the SAME lock state: find hits the locked signature.
        let get_exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(36),
            "",
            "security: User interaction is not allowed.",
        );
        let h_get = keychain_harness(&fs, &get_exec, &[("QD_HOME", "/quorum/qd")]);
        assert_eq!(
            h_get.run(|deps| get_secret(key, deps)),
            Some("sk-or-FAKE-headless".to_string())
        );
        let r = h_get.run(|deps| resolve_secret(key, "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-or-FAKE-headless".to_string()));
        assert_eq!(r.source, Some(Source::File));
    }

    /// HELD (falsification row): SAME-TIER round trips are coherent — a
    /// keychain set is read back by a keychain get; a file set is read back by
    /// a file get (the latter already pinned by
    /// `file_set_writes_toml_and_chmods_600_get_round_trips`). There is ONE
    /// shared precedence ([env →] selected backend) across every store reader;
    /// the divergence is exclusively the missing keychain→file fallthrough on
    /// a clean miss while writers can target the file tier.
    #[test]
    fn held_item1_keychain_set_then_keychain_get_round_trips() {
        let fs = FakeFs::default();
        let key = "openrouter-key";

        let set_exec =
            ScriptedExec::new().on("security", &["add-generic-password"], Some(0), "", "");
        let h_set = keychain_harness(&fs, &set_exec, &[("QD_HOME", "/quorum/qd")]);
        assert_eq!(
            h_set.run(|deps| set_secret(key, "sk-or-FAKE-kc", deps)),
            Ok(Backend::Keychain)
        );
        assert!(
            fs.files.borrow().is_empty(),
            "no file write on keychain set"
        );

        let get_exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "sk-or-FAKE-kc\n",
            "",
        );
        let h_get = keychain_harness(&fs, &get_exec, &[("QD_HOME", "/quorum/qd")]);
        assert_eq!(
            h_get.run(|deps| get_secret(key, deps)),
            Some("sk-or-FAKE-kc".to_string())
        );
    }

    /// PIN (keychain WINS over a stale file copy — the fallthrough must NOT
    /// shadow a live keychain value): both tiers hold a DIFFERENT value;
    /// keychain is selected and HITS. `get_secret` + `resolve_secret` +
    /// `resolve_config_tier` must all return the KEYCHAIN value (the file is
    /// only consulted on a clean MISS, never as an override).
    #[test]
    fn pin_item1_keychain_value_wins_over_stale_file_copy() {
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-or-FAKE-STALE-file\"\n",
        )]);
        let exec = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "sk-or-FAKE-LIVE-keychain\n",
            "",
        );
        let h = keychain_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        assert_eq!(
            h.run(|deps| get_secret("openrouter-key", deps)),
            Some("sk-or-FAKE-LIVE-keychain".to_string()),
            "keychain hit must win; the stale file copy must not shadow it"
        );
        let r = h.run(|deps| resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps));
        assert_eq!(r.value, Some("sk-or-FAKE-LIVE-keychain".to_string()));
        assert_eq!(r.source, Some(Source::Keychain));
        let c = h.run(|deps| resolve_config_tier("openrouter-key", deps));
        assert_eq!(c.source, Some(Source::Keychain));
    }

    /// AFFORDANCE PIN: `resolve_config_tier` reports the exact tier per source —
    /// env > keychain > file — and a plain file-tier key (render-default)
    /// resolves File (no env / keychain consulted).
    #[test]
    fn affordance_item1_resolve_config_tier_reports_each_tier() {
        // env tier (key in env AND file → env wins, source Env).
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-file\"\n",
        )]);
        let exec = unlocked_empty_find_exec();
        let h_env = keychain_harness(
            &fs,
            &exec,
            &[("QD_HOME", "/quorum/qd"), ("OPENROUTER_API_KEY", "sk-env")],
        );
        let r = h_env.run(|deps| resolve_config_tier("openrouter-key", deps));
        assert_eq!(r.source, Some(Source::Env));

        // keychain tier.
        let fs2 = FakeFs::default();
        let kc = ScriptedExec::new().on(
            "security",
            &["find-generic-password"],
            Some(0),
            "sk-kc\n",
            "",
        );
        let h_kc = keychain_harness(&fs2, &kc, &[("QD_HOME", "/quorum/qd")]);
        let r = h_kc.run(|deps| resolve_config_tier("openrouter-key", deps));
        assert_eq!(r.source, Some(Source::Keychain));

        // file tier (clean keychain miss → file).
        let fs3 = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-file3\"\n",
        )]);
        let miss3 = unlocked_empty_find_exec();
        let h_file = keychain_harness(&fs3, &miss3, &[("QD_HOME", "/quorum/qd")]);
        let r = h_file.run(|deps| resolve_config_tier("openrouter-key", deps));
        assert_eq!(r.source, Some(Source::File));

        // plain file-tier key: File, no env/keychain consulted.
        let fs4 = FakeFs::with(&[("/quorum/qd/config.toml", "render-default = \"alt-screen\"\n")]);
        let exec4 = ScriptedExec::new();
        let h_plain = keychain_harness(&fs4, &exec4, &[("QD_HOME", "/quorum/qd")]);
        let r = h_plain.run(|deps| resolve_config_tier("render-default", deps));
        assert_eq!(r.value, Some("alt-screen".to_string()));
        assert_eq!(r.source, Some(Source::File));

        // unset → None / None.
        let fs5 = FakeFs::default();
        let miss5 = unlocked_empty_find_exec();
        let h_unset = keychain_harness(&fs5, &miss5, &[("QD_HOME", "/quorum/qd")]);
        let r = h_unset.run(|deps| resolve_config_tier("openrouter-key", deps));
        assert_eq!((r.value, r.source), (None, None));
    }

    /// S1(i) PIN: `secret_backend_info` (qd config path) now counts an
    /// ENV-exported key even with an EMPTY store — it lists with tier=Env. This
    /// is the affordance working: config path reports WHERE a key resolves,
    /// including the env tier (was: env keys never appeared because the old
    /// enumeration only read the store).
    #[test]
    fn backend_info_lists_env_exported_key_with_empty_store_as_env() {
        let fs = FakeFs::default(); // empty store
        let exec = unlocked_empty_find_exec();
        let h = keychain_harness(
            &fs,
            &exec,
            &[
                ("QD_HOME", "/quorum/qd"),
                ("OPENROUTER_API_KEY", "sk-or-FAKE-env"),
            ],
        );
        let info = h.run(secret_backend_info);
        assert_eq!(
            info.keys_set,
            vec![("openrouter-key".to_string(), Source::Env)],
            "an env-exported key lists with tier=env even with an empty store"
        );
    }

    /// S1(ii) PIN (transparency fidelity): a STRAY hand-added `[secrets]` key —
    /// a name NOT in the registry — still appears in `qd config path` with
    /// tier=File. The known-keys enumeration alone would silently drop it
    /// (defeating "which keys are set"); the union with the file table keeps
    /// it visible.
    #[test]
    fn backend_info_lists_unknown_hand_added_secrets_key_with_tier_file() {
        let fs = FakeFs::with(&[(
            "/quorum/qd/config.toml",
            "[secrets]\nopenrouter-key = \"sk-known\"\nstray-hand-key = \"v\"\n",
        )]);
        let exec = ScriptedExec::new(); // file backend (linux)
        let h = file_harness(&fs, &exec, &[("QD_HOME", "/quorum/qd")]);
        let info = h.run(secret_backend_info);
        assert_eq!(
            info.keys_set,
            vec![
                ("openrouter-key".to_string(), Source::File),
                ("stray-hand-key".to_string(), Source::File)
            ],
            "the hand-added unknown [secrets] key must still list (tier=file)"
        );
    }
}
