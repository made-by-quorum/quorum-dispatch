//! `sb survey` — hand-parsed (spec §2: survey bypasses commander with
//! allowUnknownOption + allowExcessArguments + helpOption(false), commands/survey.ts:318-
//! 320). Port of `parseSurveyArgs` (commands/survey.ts:61-90) + `helpText` (commands/survey.ts:92-
//! 104) as a PURE function. Exit conventions (commands/survey.ts:324-331): `--help` → 0,
//! parse error → **2**, missing OpenRouter key → 1.
//!
//! A5 M5: the OpenRouter fan-out is now REAL. This bin module owns the parse +
//! the production wiring (key resolution, artifact read, curl-present check, then
//! [`dispatch::survey`]'s pure fan-out/format). The transport is `curl` via the
//! [`dispatch::exec::Exec`] seam with the Authorization header on stdin (G-S2 hygiene).

/// Default model panel (commands/survey.ts:33-39).
const DEFAULT_MODELS: &[&str] = &[
    "anthropic/claude-opus-4.7",
    "openai/gpt-5.5",
    "google/gemini-3.5-flash",
    "x-ai/grok-4.20",
    "deepseek/deepseek-v4-pro",
];

/// Default system prompt (commands/survey.ts:41-42).
const DEFAULT_SYSTEM_PROMPT: &str =
    "You are a careful reviewer. Provide clear, specific, actionable feedback on the following artifact.";

/// Parsed survey args (survey.ts SurveyArgs).
#[derive(Debug, PartialEq)]
pub struct SurveyArgs {
    pub file: Option<String>,
    pub system: String,
    pub models: Vec<String>,
}

/// `parseSurveyArgs` result (survey.ts ParseSurveyResult): exactly one of
/// `args` / `help_text` / `error` is set.
#[derive(Debug, PartialEq)]
pub enum ParseSurveyResult {
    Args(SurveyArgs),
    Help(String),
    Error(String),
}

/// Port of `parseSurveyArgs` (commands/survey.ts:61-90).
pub fn parse_survey_args(argv: &[String]) -> ParseSurveyResult {
    let mut file: Option<String> = None;
    let mut system = DEFAULT_SYSTEM_PROMPT.to_string();
    let mut models: Vec<String> = DEFAULT_MODELS.iter().map(|s| s.to_string()).collect();

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--file" => {
                if i + 1 >= argv.len() {
                    return ParseSurveyResult::Error("sb survey: --file requires a value".into());
                }
                i += 1;
                file = Some(argv[i].clone());
            }
            "--system" => {
                if i + 1 >= argv.len() {
                    return ParseSurveyResult::Error("sb survey: --system requires a value".into());
                }
                i += 1;
                system = argv[i].clone();
            }
            "--models" => {
                if i + 1 >= argv.len() {
                    return ParseSurveyResult::Error("sb survey: --models requires a value".into());
                }
                i += 1;
                models = argv[i]
                    .split(',')
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect();
            }
            "--help" | "-h" => {
                return ParseSurveyResult::Help(help_text());
            }
            other => {
                return ParseSurveyResult::Error(format!("sb survey: unknown option '{other}'"));
            }
        }
        i += 1;
    }

    if models.is_empty() {
        return ParseSurveyResult::Error("sb survey: --models resolved to an empty list".into());
    }
    ParseSurveyResult::Args(SurveyArgs {
        file,
        system,
        models,
    })
}

/// `helpText()` (commands/survey.ts:92-104).
pub fn help_text() -> String {
    format!(
        "Usage: sb survey [--file <path>] [--system <prompt>] [--models <m1,m2,...>]\n\nFan an artifact out to a panel of LLMs via OpenRouter and collect responses.\n\nOptions:\n  --file <path>       Read artifact from a file (or pipe to stdin)\n  --system <prompt>   System prompt framing the kind of response wanted\n  --models <list>     Comma-separated OpenRouter model IDs\n\nRequires OPENROUTER_API_KEY in the environment.\nDefault models: {}",
        DEFAULT_MODELS.join(", ")
    )
}

/// Production entry (commands/survey.ts:321-349 action): parse the argv tail,
/// honor the `--help`→0 / parse-error→2 conventions, then run the REAL OpenRouter
/// fan-out (A5 M5). Exit map: help→0, parse error→2, key missing / read error /
/// curl absent→1, success→0.
pub fn dispatch(argv_tail: &[String]) -> i32 {
    let args = match parse_survey_args(argv_tail) {
        ParseSurveyResult::Help(text) => {
            println!("{text}");
            return 0;
        }
        ParseSurveyResult::Error(e) => {
            eprintln!("{e}");
            return 2;
        }
        ParseSurveyResult::Args(a) => a,
    };

    // --- key resolution: env wins → tiered secret store (M2). ---
    let env = dispatch::effects::RealEnv;
    let key = match resolve_api_key(&env) {
        Ok(k) => k,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    // --- artifact: --file or stdin (survey.ts readArtifact, :273-291). ---
    let artifact = match read_artifact(args.file.as_deref()) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    // --- transport: curl must be on PATH (fresh-design divergence: TS uses
    //     fetch(); we shell out, so curl is the hard dep). ---
    let exec = dispatch::exec::RealExec;
    if !dispatch::survey::curl_available(&exec) {
        eprintln!(
            "sb survey: curl is required for the OpenRouter transport but was not found on PATH."
        );
        return 1;
    }

    // Body tempfile lives in the resolved TMPDIR (jail-honoring via the env seam).
    let tmp_dir = dispatch::effects::Env::var(&env, "TMPDIR").unwrap_or_else(|| "/tmp".to_string());
    let call = dispatch::survey::real_call_model(&exec, &tmp_dir);

    // --- fan-out + format (pure core). progress → stderr (TS log). ---
    let results =
        dispatch::survey::run_survey(&key, &args.models, &args.system, &artifact, &call, &|m| {
            eprintln!("{m}")
        });
    let report = dispatch::survey::format_results(&results, &args.system, &args.models);
    println!("{report}");
    0
}

/// Resolve the OpenRouter API key (`survey.ts resolveApiKey`, :107-130): env
/// `OPENROUTER_API_KEY` wins (cheap, always), else the tiered secret store
/// (keychain → file) via [`dispatch::secrets::resolve_secret`]. Returns the key or an
/// actionable error message.
///
/// LOCKED-KEYCHAIN FOLD-IN (spec §3.2, "survey uses the same detector"): when the
/// key is ABSENT and `resolve_secret` reports `locked == true` (env-forced
/// keychain + locked, the INACCESSIBLE-not-ABSENT case), the message says the
/// keychain is locked rather than "no key configured" — so an operator who pinned
/// `SB_SECRET_BACKEND=keychain` and hit a locked keychain is not misled into
/// thinking they never set a key.
fn resolve_api_key(env: &dispatch::effects::RealEnv) -> Result<String, String> {
    // Env always wins (without touching any backend), survey.ts:111-113.
    if let Some(v) = dispatch::effects::Env::var(env, "OPENROUTER_API_KEY") {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }

    let resolved = with_secret_deps(env, |deps| {
        dispatch::secrets::resolve_secret("openrouter-key", "OPENROUTER_API_KEY", deps)
    });
    if let Some(v) = resolved.value {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }
    if resolved.locked {
        // Same detector as config (§3.2): the null is INACCESSIBLE, not ABSENT.
        return Err(
            "sb survey: keychain is locked — a key may exist but is inaccessible \
             (SB_SECRET_BACKEND=keychain is env-forced; unlock or unset to use the \
             file fallback)."
                .to_string(),
        );
    }
    Err(
        "sb survey: No OpenRouter key. Run `sb config set openrouter-key`, or export \
         OPENROUTER_API_KEY."
            .to_string(),
    )
}

/// Build a [`dispatch::secrets::SecretDeps`] over the production seams + real-fs
/// closures and run `f`. The production analogue of TS `defaultSecretDeps`
/// (survey.ts uses the same tiered store as config) — mirrors `config.rs`'s
/// `RealStore::with_deps` exactly (chmod-600 file backend, per-process notice
/// guards). Survey only ever READS, so the write/chmod closures are the standard
/// real-fs ones (the locked-keychain fallback path may still file-read).
fn with_secret_deps<R>(
    env: &dispatch::effects::RealEnv,
    f: impl FnOnce(&dispatch::secrets::SecretDeps) -> R,
) -> R {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    let exec = dispatch::exec::RealExec;
    let notice = AtomicBool::new(false);
    let locked_diag = AtomicBool::new(false);

    let read_file = |p: &str| fs::read_to_string(p).ok();
    let write_file = |p: &str, text: &str| {
        if let Some(parent) = std::path::Path::new(p).parent() {
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
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
    };
    let file_exists = |p: &str| std::path::Path::new(p).exists();
    let keychain_available = || dispatch::secrets::real_keychain_available(&exec);
    let deps = dispatch::secrets::SecretDeps {
        platform: std::env::consts::OS,
        env,
        exec: &exec,
        keychain_available: &keychain_available,
        read_file: &read_file,
        write_file: &write_file,
        chmod: &chmod,
        file_exists: &file_exists,
        fallback_notice_emitted: &notice,
        locked_diag_emitted: &locked_diag,
    };
    f(&deps)
}

/// Read the artifact from `--file` or stdin (`survey.ts readArtifact`, :273-291).
/// File: must exist (the resolved path is echoed on miss). Stdin: a TTY with no
/// pipe → actionable error; otherwise read to EOF and trim. Empty → error.
fn read_artifact(file: Option<&str>) -> Result<String, String> {
    if let Some(path) = file {
        let resolved = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());
        return std::fs::read_to_string(path)
            .map_err(|_| format!("sb survey: file not found: {resolved}"));
    }
    // stdin: a bare TTY (no pipe) has no artifact.
    // SAFETY: isatty on a valid fd is always safe.
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 1 } {
        return Err("sb survey: no input. Pipe text to stdin or use --file <path>.".to_string());
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("sb survey: failed to read stdin: {e}"))?;
    let text = buf.trim().to_string();
    if text.is_empty() {
        return Err("sb survey: empty input.".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_when_no_args() {
        match parse_survey_args(&[]) {
            ParseSurveyResult::Args(a) => {
                assert_eq!(a.system, DEFAULT_SYSTEM_PROMPT);
                assert_eq!(
                    a.models,
                    DEFAULT_MODELS
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                );
                assert!(a.file.is_none());
            }
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[test]
    fn file_system_models_parsed() {
        match parse_survey_args(&argv(&[
            "--file",
            "spec.md",
            "--system",
            "Review for gaps",
            "--models",
            "a/b, c/d ,e/f",
        ])) {
            ParseSurveyResult::Args(a) => {
                assert_eq!(a.file.as_deref(), Some("spec.md"));
                assert_eq!(a.system, "Review for gaps");
                assert_eq!(a.models, vec!["a/b", "c/d", "e/f"]);
            }
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[test]
    fn help_returns_help_text() {
        match parse_survey_args(&argv(&["--help"])) {
            ParseSurveyResult::Help(t) => assert!(t.contains("Usage: sb survey")),
            other => panic!("expected Help, got {other:?}"),
        }
    }

    #[test]
    fn unknown_option_is_error() {
        match parse_survey_args(&argv(&["--bogus"])) {
            ParseSurveyResult::Error(e) => assert!(e.contains("unknown option")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn flag_without_value_is_error() {
        for (flag, frag) in [
            ("--file", "--file requires a value"),
            ("--system", "--system requires a value"),
            ("--models", "--models requires a value"),
        ] {
            match parse_survey_args(&argv(&[flag])) {
                ParseSurveyResult::Error(e) => assert!(e.contains(frag), "{e}"),
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    #[test]
    fn models_resolving_empty_is_error() {
        match parse_survey_args(&argv(&["--models", " , , "])) {
            ParseSurveyResult::Error(e) => assert!(e.contains("empty list")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // --- dispatch exit conventions (commands/survey.ts:324-331) ---

    #[test]
    fn dispatch_help_exit_0() {
        assert_eq!(dispatch(&argv(&["--help"])), 0);
    }

    #[test]
    fn dispatch_parse_error_exit_2() {
        assert_eq!(dispatch(&argv(&["--bogus"])), 2);
    }

    // NOTE: the valid-args path now does REAL I/O (env key resolution, stdin
    // read, curl probe), so it is NOT unit-tested here — the fan-out/format/parse
    // core is covered hermetically in `dispatch::survey`'s tests, and the live wiring is
    // a recorded gate exclusion (network + secret, G-S1).
}
