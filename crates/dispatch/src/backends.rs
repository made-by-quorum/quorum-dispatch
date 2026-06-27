//! `backends.json` + `--via <name>` backend routing (spec §3, FRESH DESIGN).
//!
//! There is NO TS counterpart at pin 8c59ec4 (verified: zero `backends` refs);
//! this is fresh design, so the code cites the spec section rather than a pin
//! file:line. The single piece of PORTED machinery it leans on is the env-file
//! foundation in [`crate::launch`] (BACKEND_ENV_KEYS, write_session_env_file,
//! session_env_prefix) — that carries its own pin citations.
//!
//! Shape (spec §3.1): a hand-edited flat file `<state_dir>/backends.json`,
//! schema v1, read ONLY when `--via` is given (additive: without `--via` the
//! file is never opened). No management verbs (flat files are the source of
//! truth, ruled (c)).
//!
//! ```json
//! {
//!   "version": 1,
//!   "backends": {
//!     "ccr-3456": {
//!       "baseUrl": "http://127.0.0.1:3456",
//!       "model": "openrouter,anthropic/claude-sonnet-4.6",
//!       "credential": { "mode": "secret", "key": "openrouter-key", "var": "ANTHROPIC_AUTH_TOKEN" }
//!     },
//!     "ccr-3457": { "baseUrl": "http://127.0.0.1:3457" }
//!   }
//! }
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::launch::BACKEND_ENV_KEYS;

/// The two BACKEND_ENV_KEYS credential slots (spec §3.1 + §3.2.3): exactly one
/// of these reaches the child. `ANTHROPIC_AUTH_TOKEN` is the default; the other
/// is allowed but mutually exclusive with it.
pub const CRED_VAR_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
pub const CRED_VAR_API_KEY: &str = "ANTHROPIC_API_KEY";

/// Credential resolution mode for a backend (spec §3.1): `secret` resolves a
/// named key via the keychain→file path; `none` injects no credential
/// (subscription / CCR-side auth). Absent credential ≡ mode `none`.
#[derive(Debug, Clone, PartialEq)]
pub enum CredMode {
    /// Resolve the named key and inject it into the credential `var` slot.
    Secret { key: String, var: String },
    /// No credential injection.
    None,
}

/// A single resolved backend profile (spec §3.1, used fields only).
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    /// `ANTHROPIC_BASE_URL` source — required, always overlaid.
    pub base_url: String,
    /// `ANTHROPIC_MODEL` source — optional, overlaid only when set.
    pub model: Option<String>,
    /// Credential injection policy.
    pub credential: CredMode,
}

/// Why resolving `--via <name>` failed. Every variant maps to a loud exit 1 that
/// names known backends only (never values) per spec §3.2.2.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendError {
    /// The file is a symlink (O_NOFOLLOW refused it) — spec §3.1 read hygiene F7.
    Symlink { path: PathBuf },
    /// The file does not exist (and `--via` was given).
    MissingFile { path: PathBuf },
    /// The file could not be read (I/O error other than ENOENT).
    ReadFailed { path: PathBuf, detail: String },
    /// The file is not valid JSON (or its top-level shape is wrong). `detail` is
    /// the parser message. Surfaces at read time (before any backend resolves).
    Unparseable { path: PathBuf, detail: String },
    /// The named backend is not present. Carries the sorted list of KNOWN names
    /// (spec §3.2.2: list known names only, never values).
    UnknownName { name: String, known: Vec<String> },
    /// The RESOLVED backend has a USED field that is wrong-typed / missing /
    /// out-of-whitelist (spec §3.1 strict-on-used). `detail` names the backend +
    /// field (never a value). Lazy: fires only when this backend is `--via`'d.
    ConfigError { detail: String },
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Symlink { path } => write!(
                f,
                "qd start --via: refusing to read backends file '{}' — it is a symlink (O_NOFOLLOW).",
                path.display()
            ),
            BackendError::MissingFile { path } => write!(
                f,
                "qd start --via: no backends file at '{}'. Create it (hand-edited JSON, schema v1) to define routing profiles.",
                path.display()
            ),
            BackendError::ReadFailed { path, detail } => write!(
                f,
                "qd start --via: failed to read backends file '{}': {detail}",
                path.display()
            ),
            BackendError::Unparseable { path, detail } => write!(
                f,
                "qd start --via: backends file '{}' is not valid JSON: {detail}",
                path.display()
            ),
            BackendError::UnknownName { name, known } => {
                if known.is_empty() {
                    write!(
                        f,
                        "qd start --via: unknown backend '{name}'. No backends are defined."
                    )
                } else {
                    write!(
                        f,
                        "qd start --via: unknown backend '{name}'. Known backends: {}.",
                        known.join(", ")
                    )
                }
            }
            BackendError::ConfigError { detail } => {
                write!(f, "qd start --via: backend is misconfigured: {detail}")
            }
        }
    }
}

impl std::error::Error for BackendError {}

// ----------------------------------------------------------------------------
// On-disk schema (serde): permissive on UNKNOWN extra fields (L8), strict on
// what we USE (spec §3.1). `#[serde(deny_unknown_fields)]` is DELIBERATELY NOT
// used so a future additive field never breaks an older binary.
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawFile {
    /// Schema version. v1 only in this release; an unknown version is tolerated
    /// (permissive read) — only the USED fields are validated strictly.
    #[allow(dead_code)]
    version: Option<u64>,
    #[serde(default)]
    backends: std::collections::BTreeMap<String, RawBackend>,
}

#[derive(Debug, Deserialize)]
struct RawBackend {
    // `baseUrl` is the only required field, but it is typed `Option` here so a
    // missing/wrong-typed value yields OUR loud ConfigError (spec §3.1) rather
    // than a serde error that names a backend's internals less precisely.
    #[serde(rename = "baseUrl")]
    base_url: Option<serde_json::Value>,
    model: Option<serde_json::Value>,
    credential: Option<RawCredential>,
}

#[derive(Debug, Deserialize)]
struct RawCredential {
    mode: Option<String>,
    key: Option<String>,
    var: Option<String>,
}

/// The parsed file: the raw name→backend map (JSON structure only). Field
/// validation is LAZY — only the RESOLVED backend's USED fields are validated
/// (spec §3.1: "strict on what it USES"). This is deliberate: a typo in one
/// backend must NOT brick routing through a different, valid backend. The loud
/// baseUrl/credential errors fire only when you actually `--via` the broken one.
#[derive(Debug)]
pub struct BackendsFile {
    backends: std::collections::BTreeMap<String, RawBackend>,
}

impl BackendsFile {
    /// The sorted list of known backend names (for the loud unknown-name error).
    pub fn known_names(&self) -> Vec<String> {
        self.backends.keys().cloned().collect()
    }

    /// Resolve + VALIDATE a backend by name. Unknown name → loud
    /// [`BackendError::UnknownName`] listing known names only (spec §3.2.2). A
    /// present-but-misconfigured backend → [`BackendError::ConfigError`] whose
    /// detail names the backend + field (spec §3.1 strict-on-used). Returns an
    /// owned [`Backend`] (the raw entry is validated into the typed form here).
    pub fn resolve(&self, name: &str) -> Result<Backend, BackendError> {
        let raw = self
            .backends
            .get(name)
            .ok_or_else(|| BackendError::UnknownName {
                name: name.to_string(),
                known: self.known_names(),
            })?;
        validate_backend(name, raw).map_err(|detail| BackendError::ConfigError { detail })
    }
}

/// The path to the backends file: `<state_dir>/backends.json` (spec §3.1,
/// SB_HOME-honored via the caller's already-resolved `SbPaths::state_dir`).
pub fn backends_file_path(state_dir: &Path) -> PathBuf {
    state_dir.join("backends.json")
}

/// Read + parse the backends file at `path` (spec §3.1). Read hygiene (F7):
///   - open with O_NOFOLLOW on the final component → a symlinked file is refused
///     loudly (never followed);
///   - if the file is group/other-readable, the returned [`PermWarning`] carries
///     a stderr WARN (names-only file, so warn not refuse) — the caller emits it.
///
/// Returns the parsed file + an optional perm warning. A missing file with
/// `--via` given is a loud error (the caller asked to route through a profile).
pub fn read_backends_file(
    path: &Path,
) -> Result<(BackendsFile, Option<PermWarning>), BackendError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    // O_NOFOLLOW: if the FINAL component is a symlink, open fails with ELOOP —
    // we refuse loudly rather than follow it (spec §3.1 F7).
    opts.custom_flags(libc::O_NOFOLLOW);

    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(e) => {
            // ELOOP (the O_NOFOLLOW symlink refusal) → the dedicated Symlink error.
            // ENOENT → MissingFile (the user asked to --via but there is no file).
            // Anything else → ReadFailed.
            return Err(match e.raw_os_error() {
                Some(code) if code == libc::ELOOP => BackendError::Symlink {
                    path: path.to_path_buf(),
                },
                _ if e.kind() == std::io::ErrorKind::NotFound => BackendError::MissingFile {
                    path: path.to_path_buf(),
                },
                _ => BackendError::ReadFailed {
                    path: path.to_path_buf(),
                    detail: e.to_string(),
                },
            });
        }
    };

    // Perm probe (F7): group/other-readable → WARN (the file holds key NAMES,
    // not values, so we warn rather than refuse). Best-effort: a metadata error
    // simply omits the warning.
    let perm_warning = group_or_other_readable(&file).then(|| PermWarning {
        path: path.to_path_buf(),
    });

    let mut text = String::new();
    if let Err(e) = file.read_to_string(&mut text) {
        return Err(BackendError::ReadFailed {
            path: path.to_path_buf(),
            detail: e.to_string(),
        });
    }

    let parsed = parse_backends(&text).map_err(|detail| BackendError::Unparseable {
        path: path.to_path_buf(),
        detail,
    })?;
    Ok((parsed, perm_warning))
}

/// A stderr WARN that the backends file is group/other-readable (spec §3.1 F7).
/// Names-only file → warn, don't refuse. The verb emits [`PermWarning::message`].
#[derive(Debug)]
pub struct PermWarning {
    path: PathBuf,
}

impl PermWarning {
    pub fn message(&self) -> String {
        format!(
            "qd start --via: WARNING: backends file '{}' is group/other-readable; \
             it holds key NAMES only, but tighten it to 0600 (chmod 600 '{}').",
            self.path.display(),
            self.path.display()
        )
    }
}

/// True iff the file's mode has any group/other read bit set (0o044).
fn group_or_other_readable(file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match file.metadata() {
        Ok(md) => md.mode() & 0o044 != 0,
        Err(_) => false,
    }
}

/// PURE: parse the backends file STRUCTURALLY into the raw name→entry map (spec
/// §3.1). JSON-syntax / top-level-shape errors surface here; per-backend USED
/// fields are validated LAZILY on [`BackendsFile::resolve`] (so one typo doesn't
/// brick routing through a valid backend). Permissive on unknown extra fields.
pub fn parse_backends(text: &str) -> Result<BackendsFile, String> {
    let raw: RawFile = serde_json::from_str(text).map_err(|e| format!("JSON parse error: {e}"))?;
    Ok(BackendsFile {
        backends: raw.backends,
    })
}

/// Strictly validate one backend's USED fields, returning a precise error naming
/// the backend + field (spec §3.1). The error text NEVER carries a value (the
/// file holds key NAMES only, but we keep the rule uniform).
fn validate_backend(name: &str, rb: &RawBackend) -> Result<Backend, String> {
    // baseUrl: REQUIRED, must be a string.
    let base_url = match &rb.base_url {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::String(_)) => {
            return Err(format!("backend '{name}' field 'baseUrl' is empty"));
        }
        Some(_) => return Err(format!("backend '{name}' field 'baseUrl' must be a string")),
        None => {
            return Err(format!(
                "backend '{name}' is missing required field 'baseUrl'"
            ))
        }
    };

    // model: OPTIONAL, must be a string when present.
    let model = match &rb.model {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(format!("backend '{name}' field 'model' must be a string")),
    };

    // credential: OPTIONAL object; absent ≡ mode none (spec §3.1).
    let credential = match &rb.credential {
        None => CredMode::None,
        Some(rc) => validate_credential(name, rc)?,
    };

    Ok(Backend {
        base_url,
        model,
        credential,
    })
}

/// Validate the credential sub-object (spec §3.1): `mode` ∈ {secret, none};
/// absent mode ≡ none; `secret` requires `key`; `var` ∈ the two credential
/// slots (default ANTHROPIC_AUTH_TOKEN). Anything else → config error.
fn validate_credential(name: &str, rc: &RawCredential) -> Result<CredMode, String> {
    let mode = rc.mode.as_deref().unwrap_or("none");
    match mode {
        "none" => Ok(CredMode::None),
        "secret" => {
            let key = match &rc.key {
                Some(k) if !k.is_empty() => k.clone(),
                _ => {
                    return Err(format!(
                        "backend '{name}' credential mode 'secret' requires a non-empty 'key'"
                    ))
                }
            };
            // var ∈ {ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY}; default AUTH_TOKEN.
            let var = match rc.var.as_deref() {
                None => CRED_VAR_AUTH_TOKEN.to_string(),
                Some(CRED_VAR_AUTH_TOKEN) => CRED_VAR_AUTH_TOKEN.to_string(),
                Some(CRED_VAR_API_KEY) => CRED_VAR_API_KEY.to_string(),
                Some(other) => {
                    return Err(format!(
                        "backend '{name}' credential 'var' must be {CRED_VAR_AUTH_TOKEN} or \
                         {CRED_VAR_API_KEY} (got '{other}')"
                    ))
                }
            };
            Ok(CredMode::Secret { key, var })
        }
        other => Err(format!(
            "backend '{name}' credential 'mode' must be 'secret' or 'none' (got '{other}')"
        )),
    }
}

/// Why composition failed (only the missing-secret case can fail; everything
/// else is deterministic overlay).
#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    /// The backend's `secret` credential key is absent in every secrets backend.
    /// Carries the backend + key NAME (spec §3.2.3 — never the value).
    SecretMissing { name: String, key: String },
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComposeError::SecretMissing { name, key } => write!(
                f,
                "qd start --via: backend '{name}' needs secret '{key}'; set it with \
                 `qd config set {key}`."
            ),
        }
    }
}

impl std::error::Error for ComposeError {}

/// PURE-ish: compose the final env-key set for `--via` (spec §3.2.3). Starts
/// from the F1 caller `captured` set, then the profile OVERLAYS WIN for the keys
/// it sets (deterministic routing — the user named the backend; caller env must
/// not silently override it):
///   - ANTHROPIC_BASE_URL ← baseUrl (always)
///   - ANTHROPIC_MODEL    ← model (when set)
///   - credential mode `secret`: `resolve_secret(key)` → value into `var` slot
///     (the caller injects the name-agnostic get_secret path; deliberately NO
///     env-override tier — profile determinism wins, red-team F3/F10);
///   - credential mode `none`: no credential injection.
///
/// Credential-slot exclusivity (F12): when the profile sets a credential var,
/// the OTHER credential slot is DROPPED from the composed set, so exactly one
/// credential reaches the child regardless of claude's own precedence.
///
/// The output preserves [`BACKEND_ENV_KEYS`] order (stable, deterministic file
/// content). `resolve_secret` is injected so this is unit-testable with no
/// keychain/file dependency; it returns the secret VALUE for a key NAME, or None.
pub fn compose_via_env(
    name: &str,
    captured: &[(String, String)],
    backend: &Backend,
    resolve_secret: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, ComposeError> {
    // Start from the captured set as a key→value map (last write wins; capture
    // already deduped, but be defensive).
    let mut map: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for (k, v) in captured {
        map.insert(k.as_str(), v.clone());
    }

    // Profile overlays WIN (spec §3.2.3).
    map.insert("ANTHROPIC_BASE_URL", backend.base_url.clone());
    if let Some(model) = &backend.model {
        map.insert("ANTHROPIC_MODEL", model.clone());
    }

    match &backend.credential {
        CredMode::None => {
            // No credential injection (subscription/CCR-side auth). We do NOT
            // touch the caller-captured credential slots here — but see the
            // note below: with mode none, whatever the caller captured rides.
        }
        CredMode::Secret { key, var } => {
            let value = resolve_secret(key).ok_or_else(|| ComposeError::SecretMissing {
                name: name.to_string(),
                key: key.clone(),
            })?;
            // Credential-slot exclusivity (F12): drop the OTHER slot so exactly
            // one credential reaches the child.
            let other = if var == CRED_VAR_AUTH_TOKEN {
                CRED_VAR_API_KEY
            } else {
                CRED_VAR_AUTH_TOKEN
            };
            map.remove(other);
            map.insert(slot_static(var), value);
        }
    }

    // Emit in BACKEND_ENV_KEYS order (deterministic).
    let mut out = Vec::new();
    for key in BACKEND_ENV_KEYS {
        if let Some(v) = map.get(key) {
            out.push((key.to_string(), v.clone()));
        }
    }
    Ok(out)
}

/// Map a validated credential var back to its &'static str so the composed map
/// keys stay `&'static` (matching the BACKEND_ENV_KEYS literals).
fn slot_static(var: &str) -> &'static str {
    if var == CRED_VAR_API_KEY {
        CRED_VAR_API_KEY
    } else {
        CRED_VAR_AUTH_TOKEN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_secret(_k: &str) -> Option<String> {
        None
    }

    // --- parse: permissive on unknown, strict on used (spec §3.1) ---

    #[test]
    fn parse_minimal_baseurl_only() {
        let f =
            parse_backends(r#"{"version":1,"backends":{"a":{"baseUrl":"http://x:1"}}}"#).unwrap();
        let b = f.resolve("a").unwrap();
        assert_eq!(b.base_url, "http://x:1");
        assert_eq!(b.model, None);
        assert_eq!(b.credential, CredMode::None);
    }

    #[test]
    fn parse_full_secret_profile() {
        let f = parse_backends(
            r#"{"version":1,"backends":{"ccr":{"baseUrl":"http://127.0.0.1:3456",
               "model":"openrouter,anthropic/claude-sonnet-4.6",
               "credential":{"mode":"secret","key":"openrouter-key","var":"ANTHROPIC_AUTH_TOKEN"}}}}"#,
        )
        .unwrap();
        let b = f.resolve("ccr").unwrap();
        assert_eq!(b.base_url, "http://127.0.0.1:3456");
        assert_eq!(
            b.model.as_deref(),
            Some("openrouter,anthropic/claude-sonnet-4.6")
        );
        assert_eq!(
            b.credential,
            CredMode::Secret {
                key: "openrouter-key".to_string(),
                var: CRED_VAR_AUTH_TOKEN.to_string()
            }
        );
    }

    #[test]
    fn parse_permissive_on_unknown_fields() {
        // Unknown top-level + per-backend fields are ignored (L8), used fields parse.
        let f = parse_backends(
            r#"{"version":1,"note":"hi","backends":{"a":{"baseUrl":"http://x:1","futureField":true}}}"#,
        )
        .unwrap();
        assert_eq!(f.resolve("a").unwrap().base_url, "http://x:1");
    }

    #[test]
    fn parse_credential_var_defaults_to_auth_token() {
        let f = parse_backends(
            r#"{"backends":{"a":{"baseUrl":"http://x:1","credential":{"mode":"secret","key":"k"}}}}"#,
        )
        .unwrap();
        match &f.resolve("a").unwrap().credential {
            CredMode::Secret { var, .. } => assert_eq!(var, CRED_VAR_AUTH_TOKEN),
            other => panic!("expected secret, got {other:?}"),
        }
    }

    #[test]
    fn parse_credential_absent_is_mode_none() {
        let f = parse_backends(r#"{"backends":{"a":{"baseUrl":"http://x:1"}}}"#).unwrap();
        assert_eq!(f.resolve("a").unwrap().credential, CredMode::None);
    }

    #[test]
    fn parse_credential_mode_none_explicit() {
        let f = parse_backends(
            r#"{"backends":{"a":{"baseUrl":"http://x:1","credential":{"mode":"none"}}}}"#,
        )
        .unwrap();
        assert_eq!(f.resolve("a").unwrap().credential, CredMode::None);
    }

    // --- resolve: STRICT (LAZY) failures on USED fields (spec §3.1) ---
    // These fire at resolve() time (lazy), not parse() time, so a typo in one
    // backend never bricks routing through a valid sibling (proven below).

    /// Helper: the ConfigError detail string for a resolve failure.
    fn resolve_err(json: &str, name: &str) -> String {
        match parse_backends(json).unwrap().resolve(name).unwrap_err() {
            BackendError::ConfigError { detail } => detail,
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn resolve_missing_baseurl_errors_naming_field() {
        let e = resolve_err(r#"{"backends":{"a":{"model":"m"}}}"#, "a");
        assert!(e.contains("'a'") && e.contains("baseUrl"), "{e}");
    }

    #[test]
    fn resolve_wrong_typed_baseurl_errors() {
        let e = resolve_err(r#"{"backends":{"a":{"baseUrl":1234}}}"#, "a");
        assert!(e.contains("baseUrl") && e.contains("string"), "{e}");
    }

    #[test]
    fn resolve_unknown_mode_errors() {
        let e = resolve_err(
            r#"{"backends":{"a":{"baseUrl":"http://x:1","credential":{"mode":"vault"}}}}"#,
            "a",
        );
        assert!(e.contains("mode") && e.contains("vault"), "{e}");
    }

    #[test]
    fn resolve_out_of_whitelist_var_errors() {
        let e = resolve_err(
            r#"{"backends":{"a":{"baseUrl":"http://x:1","credential":{"mode":"secret","key":"k","var":"PATH"}}}}"#,
            "a",
        );
        assert!(e.contains("var") && e.contains("PATH"), "{e}");
    }

    #[test]
    fn resolve_secret_without_key_errors() {
        let e = resolve_err(
            r#"{"backends":{"a":{"baseUrl":"http://x:1","credential":{"mode":"secret"}}}}"#,
            "a",
        );
        assert!(e.contains("key"), "{e}");
    }

    #[test]
    fn parse_invalid_json_errors() {
        assert!(parse_backends("{not json").is_err());
    }

    #[test]
    fn resolve_lazy_one_bad_backend_does_not_brick_a_valid_sibling() {
        // SPEC §3.1 strict-on-USED + the design decision: a typo in `bad` (missing
        // baseUrl) must NOT prevent routing through the valid `good`. parse() is
        // structural-only; only the RESOLVED backend is validated.
        let f =
            parse_backends(r#"{"backends":{"good":{"baseUrl":"http://x:1"},"bad":{"model":"m"}}}"#)
                .unwrap();
        // `good` resolves fine despite `bad` being misconfigured.
        assert_eq!(f.resolve("good").unwrap().base_url, "http://x:1");
        // `bad` is the one that fails loudly (lazy).
        assert!(matches!(
            f.resolve("bad").unwrap_err(),
            BackendError::ConfigError { .. }
        ));
    }

    // --- resolve: unknown name lists KNOWN names only (spec §3.2.2) ---

    #[test]
    fn resolve_unknown_lists_known_names() {
        let f = parse_backends(
            r#"{"backends":{"ccr-a":{"baseUrl":"http://x:1"},"ccr-b":{"baseUrl":"http://x:2"}}}"#,
        )
        .unwrap();
        match f.resolve("nope").unwrap_err() {
            BackendError::UnknownName { name, known } => {
                assert_eq!(name, "nope");
                assert_eq!(known, vec!["ccr-a".to_string(), "ccr-b".to_string()]);
            }
            other => panic!("expected UnknownName, got {other:?}"),
        }
    }

    #[test]
    fn unknown_name_message_carries_no_values() {
        let f = parse_backends(r#"{"backends":{"ccr-a":{"baseUrl":"http://secret-host:9999"}}}"#)
            .unwrap();
        let msg = f.resolve("nope").unwrap_err().to_string();
        assert!(msg.contains("ccr-a"));
        // The baseUrl VALUE must never appear in the error.
        assert!(!msg.contains("secret-host"), "value leaked: {msg}");
    }

    // --- read_backends_file: read hygiene F7 (O_NOFOLLOW + perm WARN) ---

    #[test]
    fn read_missing_file_is_missing_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backends.json");
        match read_backends_file(&path).unwrap_err() {
            BackendError::MissingFile { .. } => {}
            other => panic!("expected MissingFile, got {other:?}"),
        }
    }

    #[test]
    fn read_real_file_ok_when_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backends.json");
        std::fs::write(&path, r#"{"backends":{"a":{"baseUrl":"http://x:1"}}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (file, warn) = read_backends_file(&path).unwrap();
        assert_eq!(file.resolve("a").unwrap().base_url, "http://x:1");
        assert!(warn.is_none(), "0600 file must not warn");
    }

    #[test]
    fn read_symlink_is_refused_loudly() {
        // F7: O_NOFOLLOW refuses a symlinked backends.json (never follow it).
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        std::fs::write(&real, r#"{"backends":{}}"#).unwrap();
        let link = dir.path().join("backends.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        match read_backends_file(&link).unwrap_err() {
            BackendError::Symlink { .. } => {}
            other => panic!("expected Symlink refusal, got {other:?}"),
        }
    }

    #[test]
    fn read_group_readable_file_warns_not_refuses() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backends.json");
        std::fs::write(&path, r#"{"backends":{"a":{"baseUrl":"http://x:1"}}}"#).unwrap();
        // group-readable (0o640) → WARN, but still usable (names-only file).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let (file, warn) = read_backends_file(&path).unwrap();
        assert_eq!(file.resolve("a").unwrap().base_url, "http://x:1");
        let warn = warn.expect("group-readable must warn");
        assert!(warn.message().contains("group/other-readable"));
        assert!(warn.message().contains("chmod 600"));
    }

    // --- compose: profile-wins overlay (spec §3.2.3) ---

    #[test]
    fn compose_baseurl_always_overlays_caller() {
        // Caller captured a DIFFERENT base url; the profile wins.
        let captured = vec![(
            "ANTHROPIC_BASE_URL".to_string(),
            "http://caller:1".to_string(),
        )];
        let b = Backend {
            base_url: "http://profile:2".to_string(),
            model: None,
            credential: CredMode::None,
        };
        let out = compose_via_env("p", &captured, &b, &no_secret).unwrap();
        assert_eq!(
            out,
            vec![(
                "ANTHROPIC_BASE_URL".to_string(),
                "http://profile:2".to_string()
            )]
        );
    }

    #[test]
    fn compose_model_overlays_only_when_set() {
        let captured = vec![("ANTHROPIC_MODEL".to_string(), "caller-model".to_string())];
        // Profile has NO model → caller's model rides (no overlay).
        let b = Backend {
            base_url: "http://x:1".to_string(),
            model: None,
            credential: CredMode::None,
        };
        let out = compose_via_env("p", &captured, &b, &no_secret).unwrap();
        // BACKEND_ENV_KEYS order: BASE_URL, API_KEY, AUTH_TOKEN, MODEL.
        assert_eq!(
            out,
            vec![
                ("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string()),
                ("ANTHROPIC_MODEL".to_string(), "caller-model".to_string()),
            ]
        );
    }

    #[test]
    fn compose_secret_value_into_var_slot() {
        let secret = |k: &str| (k == "openrouter-key").then(|| "sk-FAKE-test".to_string());
        let b = Backend {
            base_url: "http://x:1".to_string(),
            model: None,
            credential: CredMode::Secret {
                key: "openrouter-key".to_string(),
                var: CRED_VAR_AUTH_TOKEN.to_string(),
            },
        };
        let out = compose_via_env("p", &[], &b, &secret).unwrap();
        assert!(out.contains(&(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "sk-FAKE-test".to_string()
        )));
        assert!(out.contains(&("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string())));
    }

    #[test]
    fn compose_secret_missing_is_loud_error_naming_key() {
        let b = Backend {
            base_url: "http://x:1".to_string(),
            model: None,
            credential: CredMode::Secret {
                key: "openrouter-key".to_string(),
                var: CRED_VAR_AUTH_TOKEN.to_string(),
            },
        };
        let e = compose_via_env("ccr", &[], &b, &no_secret).unwrap_err();
        match &e {
            ComposeError::SecretMissing { name, key } => {
                assert_eq!(name, "ccr");
                assert_eq!(key, "openrouter-key");
            }
        }
        // The message names the key + config-set hint, never a value.
        let msg = e.to_string();
        assert!(msg.contains("openrouter-key") && msg.contains("qd config set"));
    }

    // --- compose: credential-slot exclusivity (F12, spec §3.2.3) ---

    #[test]
    fn compose_drops_other_credential_slot_auth_wins() {
        // Caller captured an API_KEY; profile sets the AUTH_TOKEN slot via secret.
        // The OTHER slot (API_KEY) must be DROPPED so exactly one credential rides.
        let captured = vec![("ANTHROPIC_API_KEY".to_string(), "caller-api".to_string())];
        let secret = |_k: &str| Some("sk-FAKE-profile".to_string());
        let b = Backend {
            base_url: "http://x:1".to_string(),
            model: None,
            credential: CredMode::Secret {
                key: "k".to_string(),
                var: CRED_VAR_AUTH_TOKEN.to_string(),
            },
        };
        let out = compose_via_env("p", &captured, &b, &secret).unwrap();
        assert!(
            !out.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"),
            "API_KEY slot must be dropped"
        );
        assert!(out.contains(&(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "sk-FAKE-profile".to_string()
        )));
    }

    #[test]
    fn compose_drops_other_credential_slot_api_wins() {
        // Symmetric: profile uses the API_KEY slot → AUTH_TOKEN dropped.
        let captured = vec![(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "caller-auth".to_string(),
        )];
        let secret = |_k: &str| Some("sk-FAKE-profile".to_string());
        let b = Backend {
            base_url: "http://x:1".to_string(),
            model: None,
            credential: CredMode::Secret {
                key: "k".to_string(),
                var: CRED_VAR_API_KEY.to_string(),
            },
        };
        let out = compose_via_env("p", &captured, &b, &secret).unwrap();
        assert!(
            !out.iter().any(|(k, _)| k == "ANTHROPIC_AUTH_TOKEN"),
            "AUTH_TOKEN slot must be dropped"
        );
        assert!(out.contains(&(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-FAKE-profile".to_string()
        )));
    }

    #[test]
    fn compose_mode_none_no_credential_injection() {
        // mode none: no secret resolution, the captured set rides unchanged
        // (subscription/CCR-side auth). resolve_secret must NOT be consulted.
        let captured = vec![(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "caller-auth".to_string(),
        )];
        let panic_secret =
            |_k: &str| -> Option<String> { panic!("must not resolve a secret in mode none") };
        let b = Backend {
            base_url: "http://x:1".to_string(),
            model: None,
            credential: CredMode::None,
        };
        let out = compose_via_env("p", &captured, &b, &panic_secret).unwrap();
        // Caller-captured AUTH_TOKEN rides (mode none does not touch it).
        assert!(out.contains(&(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "caller-auth".to_string()
        )));
        assert!(out.contains(&("ANTHROPIC_BASE_URL".to_string(), "http://x:1".to_string())));
    }
}
