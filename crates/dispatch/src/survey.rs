//! `qd survey` core — the OpenRouter fan-out (Phase G), ported from
//! `0d0fa9e:src/commands/survey.ts`. The arg parse + help layer lives in the bin
//! (`bin/dispatch/survey.rs`, M1); THIS module is the A5 hole made real: request
//! construction, the per-model call, fan-out, response parsing, and formatting.
//!
//! TESTABILITY (mirrors the TS `SurveyDeps.callModel` seam, survey.ts:62-77): the
//! OpenRouter call is an INJECTED effect ([`CallModel`]). Request construction,
//! fan-out (`Promise.allSettled` semantics → per-model failure isolation),
//! parsing, and formatting are all unit-tested with a fake — NO network, NO
//! secret. The real call ([`real_call_model`]) shells out to `curl` through the
//! [`Exec`] seam.
//!
//! KEY HYGIENE (red-team R3, BINDING — G-S2): the Authorization header is NEVER
//! an argv token (`-H "Authorization: Bearer K"` is `ps`-visible on a shared
//! host, ADD-10). It is written to curl's STDIN via `curl --config -`; the
//! request body goes to a chmod-600 tempfile referenced by `--data-binary @file`.
//! argv carries ONLY the URL + static flags. See [`build_curl_config`] /
//! [`build_curl_argv`].

use crate::exec::Exec;
use std::sync::Mutex;

/// OpenRouter chat-completions endpoint (`survey.ts:44`).
pub const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Per-model hard timeout (`survey.ts:46`): 120s.
pub const MODEL_TIMEOUT_MS: u64 = 120_000;

/// `max_tokens` for every request (`survey.ts:47`).
pub const MAX_TOKENS: u64 = 16384;

/// One model's result (`survey.ts ModelResult`, :120-128). `error` set ⇒ the row
/// is a FAILED row; otherwise `response` carries the model's text.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResult {
    pub model: String,
    pub response: String,
    pub duration_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub error: Option<String>,
}

impl ModelResult {
    /// A failed row (TS sets every count to 0 + the error string).
    fn failed(model: &str, duration_ms: u64, error: String) -> Self {
        ModelResult {
            model: model.to_string(),
            response: String::new(),
            duration_ms,
            prompt_tokens: 0,
            completion_tokens: 0,
            error: Some(error),
        }
    }
}

/// The injected effect: call ONE model. Production binds [`real_call_model`];
/// tests pass a fake (TS `CallModel`, survey.ts:131-136). Takes the key, model,
/// system prompt, artifact; returns a [`ModelResult`]. It NEVER panics — a
/// transport failure is reported as a failed `ModelResult` (mirrors TS, whose
/// `realCallModel` catches and returns an error row).
pub type CallModel<'a> = dyn Fn(&str, &str, &str, &str) -> ModelResult + Sync + 'a;

// ----------------------------------------------------------------------------
// Request construction (pure — unit-testable, survey.ts:139-160).
// ----------------------------------------------------------------------------

/// Build the exact OpenRouter request BODY JSON (`survey.ts buildRequest`,
/// :139-160 body half). Pure: serde_json produces the same shape the TS
/// `JSON.stringify` does (model, messages[system,user], max_tokens). The headers
/// (incl. the secret Authorization) are NOT here — they go to the curl config
/// (key hygiene).
pub fn build_request_body(model: &str, system_prompt: &str, artifact: &str) -> String {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": artifact },
        ],
        "max_tokens": MAX_TOKENS,
    });
    body.to_string()
}

/// Build the curl STDIN config (`curl --config -`). This is where the SECRET
/// lives: the Authorization header + the other request headers, written to the
/// child's stdin so they never touch argv (G-S2). curl config syntax: one
/// `key = "value"` per line.
///
/// The other static headers (Content-Type / HTTP-Referer / X-Title,
/// `survey.ts:148-153`) ride along in the config — no reason to put them on argv,
/// and keeping them together keeps the request shape in one place.
pub fn build_curl_config(api_key: &str) -> String {
    // curl config quoting: double-quoted values honor backslash escapes. The key
    // is opaque; escape `\` and `"` so a key containing them can't break out of
    // the quoted value (defense-in-depth — OpenRouter keys are alnum, but never
    // trust the byte shape of a secret).
    let escaped_key = api_key.replace('\\', "\\\\").replace('"', "\\\"");
    let mut cfg = String::new();
    cfg.push_str(&format!(
        "header = \"Authorization: Bearer {escaped_key}\"\n"
    ));
    cfg.push_str("header = \"Content-Type: application/json\"\n");
    cfg.push_str("header = \"HTTP-Referer: https://github.com/private-org/switchboard\"\n");
    cfg.push_str("header = \"X-Title: qd survey\"\n");
    cfg
}

/// Build the curl ARGV — URL + static flags ONLY, NEVER the secret (G-S2). The
/// body is passed by reference to a chmod-600 tempfile (`--data-binary @path`);
/// the config (with the Authorization header) comes via stdin (`--config -`).
///
/// `-sS` (silent but show errors), `-m 120` (per-model deadline, also belt-and-
/// suspenders with the Exec timeout), `-X POST`.
pub fn build_curl_argv(body_tempfile: &str) -> Vec<String> {
    vec![
        "-sS".to_string(),
        "-m".to_string(),
        "120".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        "--config".to_string(),
        "-".to_string(),
        "--data-binary".to_string(),
        format!("@{body_tempfile}"),
        "-w".to_string(),
        // Append the HTTP status on its own trailing line so the parser can split
        // it off (curl `-w` writes after the body). `%{http_code}` is curl's
        // status token.
        "\n%{http_code}".to_string(),
        OPENROUTER_API_URL.to_string(),
    ]
}

// ----------------------------------------------------------------------------
// Response parsing (pure — survey.ts:163-194 `parseModelResponse`).
// ----------------------------------------------------------------------------

/// Parse an OpenRouter HTTP response into a [`ModelResult`] (`survey.ts
/// parseModelResponse`, :163-194). Pure given the raw pieces — no real fetch.
///
/// Branches (TS order): non-2xx → `HTTP <status>: <body[..500]>`; invalid JSON →
/// `invalid JSON: <msg>`; `data.error` present → its `.message` (or stringified);
/// else success → `choices[0].message.content` (or "(empty response)") + usage
/// token extraction.
pub fn parse_model_response(
    model: &str,
    duration_ms: u64,
    http_ok: bool,
    http_status: u16,
    body_text: &str,
) -> ModelResult {
    if !http_ok {
        let snippet: String = body_text.chars().take(500).collect();
        return ModelResult::failed(model, duration_ms, format!("HTTP {http_status}: {snippet}"));
    }
    let data: serde_json::Value = match serde_json::from_str(body_text) {
        Ok(v) => v,
        Err(e) => {
            return ModelResult::failed(model, duration_ms, format!("invalid JSON: {e}"));
        }
    };
    if let Some(err) = data.get("error") {
        // TS: data.error.message || JSON.stringify(data.error).
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return ModelResult::failed(model, duration_ms, msg);
    }
    let response = data
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("(empty response)")
        .to_string();
    let usage = data.get("usage");
    let prompt_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    ModelResult {
        model: model.to_string(),
        response,
        duration_ms,
        prompt_tokens,
        completion_tokens,
        error: None,
    }
}

// ----------------------------------------------------------------------------
// Fan-out (`Promise.allSettled` semantics — survey.ts:197-226, red-team R4).
// ----------------------------------------------------------------------------

/// Fan an artifact out to every model, collecting one [`ModelResult`] PER model.
/// Per-model failure isolation (TS `Promise.allSettled`, :199-201): one bad model
/// NEVER aborts the panel — every requested model produces a row, error or not.
/// Scoped threads run the calls concurrently; a panicking call (the analogue of a
/// REJECTED promise, TS :218-220) still becomes a FAILED row so the report
/// accounts for every model.
///
/// `log` receives a per-model progress line on stderr (TS `log(...)`).
pub fn run_survey(
    api_key: &str,
    models: &[String],
    system_prompt: &str,
    artifact: &str,
    call_model: &CallModel,
    log: &dyn Fn(&str),
) -> Vec<ModelResult> {
    log(&format!(
        "Sending artifact ({} chars) to {} models...",
        artifact.chars().count(),
        models.len()
    ));

    // Collect results positionally so a panicking call maps back to its model.
    let slots: Vec<Mutex<Option<ModelResult>>> = models.iter().map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for (i, model) in models.iter().enumerate() {
            let slot = &slots[i];
            let cm = &call_model;
            scope.spawn(move || {
                // Catch a panic so one model's blow-up never poisons the panel
                // (the REJECTED-promise analogue). The slot stays `None` ⇒ the
                // post-loop fills a failed row.
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cm(api_key, model, system_prompt, artifact)
                }));
                if let Ok(r) = res {
                    *slot.lock().unwrap() = Some(r);
                }
            });
        }
    });

    let mut results = Vec::with_capacity(models.len());
    for (i, slot) in slots.into_iter().enumerate() {
        let r = slot.into_inner().unwrap().unwrap_or_else(|| {
            // The call panicked (rejected) — a FAILED row keeps the panel whole.
            ModelResult::failed(&models[i], 0, "(rejected)".to_string())
        });
        log(&format!(
            "  {}: {} ({}ms)",
            if r.error.is_some() { "Failed" } else { "Done" },
            r.model,
            r.duration_ms
        ));
        results.push(r);
    }
    results
}

// ----------------------------------------------------------------------------
// Formatting (byte-shape port — survey.ts:229-271 `formatResults`).
// ----------------------------------------------------------------------------

/// Format the panel results into the report (`survey.ts formatResults`,
/// :229-271). Byte-shape port: the 80-char `=` / `-` banners, the `SURVEY` /
/// `SUMMARY` headers, per-model sections, and the right-aligned summary table.
pub fn format_results(results: &[ModelResult], system_prompt: &str, models: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let bar80 = "=".repeat(80);
    let dash80 = "-".repeat(80);

    lines.push(bar80.clone());
    lines.push("SURVEY".to_string());
    lines.push(bar80.clone());
    lines.push(String::new());
    lines.push(format!("Panel: {}", models.join(", ")));
    lines.push(format!("System prompt: {system_prompt}"));
    lines.push(String::new());

    for r in results {
        lines.push(dash80.clone());
        lines.push(format!("MODEL: {}", r.model));
        lines.push(dash80.clone());
        lines.push(match &r.error {
            Some(e) => format!("[ERROR] {e}"),
            None => r.response.clone(),
        });
        lines.push(String::new());
    }

    lines.push(bar80.clone());
    lines.push("SUMMARY".to_string());
    lines.push(bar80);
    lines.push(String::new());

    // nameWidth = max(5, longest model name) — TS Math.max(5, ...lengths).
    let name_width = results
        .iter()
        .map(|r| r.model.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    let header = format!(
        "{}  {}  {}  {}  Status",
        pad_end("Model", name_width),
        pad_start("Time", 8),
        pad_start("Prompt", 8),
        pad_start("Completion", 10),
    );
    let header_len = header.chars().count();
    lines.push(header);
    lines.push("-".repeat(header_len));
    for r in results {
        let time = if r.duration_ms < 1000 {
            format!("{}ms", r.duration_ms)
        } else {
            format!("{:.1}s", r.duration_ms as f64 / 1000.0)
        };
        let status = if r.error.is_some() { "FAILED" } else { "OK" };
        lines.push(format!(
            "{}  {}  {}  {}  {}",
            pad_end(&r.model, name_width),
            pad_start(&time, 8),
            pad_start(&r.prompt_tokens.to_string(), 8),
            pad_start(&r.completion_tokens.to_string(), 10),
            status,
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// JS `String.padEnd(width)` — left-justify in a field at least `width` wide
/// (never truncates). Counts by char (the model ids are ASCII, but be precise).
fn pad_end(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// JS `String.padStart(width)` — right-justify in a field at least `width` wide.
fn pad_start(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - n))
    }
}

// ----------------------------------------------------------------------------
// Production effect: real curl call through the Exec seam (G-S2 transport).
// ----------------------------------------------------------------------------

/// The result of trying to build the production [`CallModel`]: either the curl
/// transport, or an actionable error (curl absent).
pub enum TransportError {
    /// `curl` is not on PATH — survey can't run (`survey.ts` assumes fetch; our
    /// fresh-design divergence shells out, so curl is the hard dep).
    CurlAbsent,
}

/// Probe whether `curl` is invocable (spawn `curl --version`). A spawn error
/// (ENOENT) ⇒ absent. Mirrors `real_keychain_available`'s probe shape.
pub fn curl_available(exec: &dyn Exec) -> bool {
    exec.run("curl", &["--version".to_string()], &[], None, None)
        .is_ok()
}

/// Build the production [`real_call_model`] closure over the Exec seam + a tmp
/// dir for the chmod-600 body file. The closure performs ONE model call:
///   1. write the request body to a chmod-600 tempfile in `tmp_dir`,
///   2. invoke `curl` with the static argv + the config (Authorization) on stdin,
///   3. split the trailing `\n<http_code>` line, parse the response,
///   4. delete the tempfile.
///
/// A transport-level failure (non-zero curl with no body, timeout) becomes a
/// FAILED [`ModelResult`] — never a panic (allSettled discipline).
pub fn real_call_model<'a, E: Exec + Sync + 'a>(
    exec: &'a E,
    tmp_dir: &'a str,
) -> impl Fn(&str, &str, &str, &str) -> ModelResult + Sync + 'a {
    move |api_key: &str, model: &str, system_prompt: &str, artifact: &str| {
        let start = std::time::Instant::now();
        let body = build_request_body(model, system_prompt, artifact);

        // Unique-ish body tempfile (pid + nanos + a sanitized model fragment).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let safe_model: String = model
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let body_path = format!(
            "{}/qd-survey-{}-{}-{}.json",
            tmp_dir.trim_end_matches('/'),
            std::process::id(),
            nanos,
            safe_model
        );

        if let Err(e) = write_chmod600(&body_path, body.as_bytes()) {
            return ModelResult::failed(
                model,
                start.elapsed().as_millis() as u64,
                format!("could not stage request body: {e}"),
            );
        }

        let argv = build_curl_argv(&body_path);
        let config = build_curl_config(api_key);
        let outcome = exec.run_with_stdin(
            "curl",
            &argv,
            &[],
            None,
            Some(MODEL_TIMEOUT_MS),
            config.as_bytes(),
        );
        // Best-effort cleanup of the secret-adjacent body file.
        let _ = std::fs::remove_file(&body_path);

        let duration_ms = start.elapsed().as_millis() as u64;
        match outcome {
            Err(e) => ModelResult::failed(model, duration_ms, format!("curl spawn failed: {e}")),
            Ok(res) if res.timed_out => {
                ModelResult::failed(model, duration_ms, "request timed out".to_string())
            }
            Ok(res) => {
                // curl `-w "\n%{http_code}"` appended the status on the last line.
                let (body_text, status) = split_trailing_status(&res.stdout);
                // A curl that emitted nothing + non-zero exit is a transport
                // failure (no HTTP at all) — surface curl's stderr.
                if status == 0 {
                    let stderr = res.stderr.trim();
                    let msg = if stderr.is_empty() {
                        "no response from curl".to_string()
                    } else {
                        stderr.to_string()
                    };
                    return ModelResult::failed(model, duration_ms, format!("curl: {msg}"));
                }
                let http_ok = (200..300).contains(&status);
                parse_model_response(model, duration_ms, http_ok, status, body_text)
            }
        }
    }
}

/// Split curl's `-w "\n%{http_code}"` trailing status off the captured stdout.
/// Returns `(body_without_status, http_status)`; status 0 if the trailer is
/// missing/unparseable (a transport-level failure — no HTTP happened).
fn split_trailing_status(stdout: &str) -> (&str, u16) {
    match stdout.rfind('\n') {
        Some(idx) => {
            let (body, tail) = stdout.split_at(idx);
            let status = tail.trim().parse::<u16>().unwrap_or(0);
            (body, status)
        }
        None => {
            // No newline: the whole thing is either the status (no body) or junk.
            (stdout, stdout.trim().parse::<u16>().unwrap_or(0))
        }
    }
}

/// Write `bytes` to `path`, creating it 0600 (the body file holds the artifact —
/// not the key, but it lives in a shared TMPDIR, so lock it down). chmod 600 is
/// enforced after the write regardless of umask.
fn write_chmod600(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut fh = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    fh.write_all(bytes)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;

    fn models(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // --- build_request_body (pure) ---

    #[test]
    fn body_has_model_messages_and_max_tokens() {
        let b = build_request_body("a/b", "be careful", "the artifact");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["model"], "a/b");
        assert_eq!(v["max_tokens"], 16384);
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][0]["content"], "be careful");
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "the artifact");
    }

    // --- G-S2: key hygiene. The secret is in the config (stdin), NOT in argv. ---

    /// THE G-S2 hygiene check, shared verbatim by the positive test AND the
    /// negative control (ADD-8 red-team NIT V3b: a control that re-implements
    /// the assert can't catch drift in it). Returns the first offending token.
    fn argv_hygiene_violation(argv: &[String], secret: &str) -> Option<String> {
        argv.iter()
            .find(|tok| tok.contains(secret) || tok.contains("Authorization"))
            .cloned()
    }

    #[test]
    fn curl_argv_carries_no_secret_substring() {
        let argv = build_curl_argv("/jail/tmp/body.json");
        let secret = "sk-or-SUPERSECRET-KEY-123";
        // The key (or any Authorization header) is never an argv token.
        if let Some(tok) = argv_hygiene_violation(&argv, secret) {
            panic!("secret/Authorization leaked into argv token: {tok}");
        }
        // argv carries only static flags + the URL + the body @file ref.
        assert!(argv.contains(&OPENROUTER_API_URL.to_string()));
        assert!(argv.contains(&"--config".to_string()));
        assert!(argv.contains(&"-".to_string()));
        assert!(argv.contains(&"@/jail/tmp/body.json".to_string()));
    }

    #[test]
    fn curl_config_carries_the_secret_on_stdin() {
        let cfg = build_curl_config("sk-or-SUPERSECRET-KEY-123");
        assert!(cfg.contains("Authorization: Bearer sk-or-SUPERSECRET-KEY-123"));
        assert!(cfg.contains("X-Title: qd survey"));
        assert!(cfg.contains("HTTP-Referer: https://github.com/private-org/switchboard"));
    }

    #[test]
    fn curl_config_escapes_quotes_and_backslashes_in_key() {
        // Defense-in-depth: a key with a quote can't break out of the config
        // value. (OpenRouter keys are alnum; never trust a secret's bytes.)
        let cfg = build_curl_config("ab\"c\\d");
        assert!(cfg.contains("Authorization: Bearer ab\\\"c\\\\d"));
    }

    // --- G-S2 NEGATIVE CONTROL (teeth): the argv-token variant MUST red. ---
    // This proves the hygiene assert has teeth — if someone regresses to
    // `-H "Authorization: Bearer K"`, the assertion the gate relies on fails.

    /// The BANNED shape: header as an argv `-H` token. Kept ONLY so the negative
    /// control can demonstrate the hygiene assert catches it.
    fn build_curl_argv_insecure_header_in_argv(api_key: &str, body_tempfile: &str) -> Vec<String> {
        vec![
            "-sS".to_string(),
            "-H".to_string(),
            format!("Authorization: Bearer {api_key}"),
            "--data-binary".to_string(),
            format!("@{body_tempfile}"),
            OPENROUTER_API_URL.to_string(),
        ]
    }

    #[test]
    fn negative_control_argv_token_variant_would_fail_the_hygiene_assert() {
        let secret = "sk-or-SUPERSECRET-KEY-123";
        let insecure = build_curl_argv_insecure_header_in_argv(secret, "/jail/tmp/body.json");
        // The EXACT check the secure path passes (argv_hygiene_violation — the
        // shared fn, not a re-implementation) must FIRE on the insecure argv.
        // If the real check ever drifts to where it misses this leak, THIS
        // control reds too (red-team NIT V3b closed).
        assert!(
            argv_hygiene_violation(&insecure, secret).is_some(),
            "negative control is meant to leak the secret into argv — if the \
             shared hygiene check does not fire here, it has no teeth"
        );
    }

    // --- parse_model_response (pure, every branch) ---

    #[test]
    fn parse_http_error_branch() {
        let r = parse_model_response("m", 5, false, 500, "boom");
        assert_eq!(r.error.as_deref(), Some("HTTP 500: boom"));
        assert!(r.response.is_empty());
    }

    #[test]
    fn parse_http_error_truncates_body_to_500_chars() {
        let big = "x".repeat(900);
        let r = parse_model_response("m", 0, false, 502, &big);
        let e = r.error.unwrap();
        // "HTTP 502: " + 500 x's.
        assert_eq!(e, format!("HTTP 502: {}", "x".repeat(500)));
    }

    #[test]
    fn parse_invalid_json_branch() {
        let r = parse_model_response("m", 1, true, 200, "{not json");
        assert!(r.error.unwrap().starts_with("invalid JSON:"));
    }

    #[test]
    fn parse_api_error_message_branch() {
        let r = parse_model_response("m", 1, true, 200, r#"{"error":{"message":"rate limited"}}"#);
        assert_eq!(r.error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn parse_api_error_without_message_stringifies() {
        let r = parse_model_response("m", 1, true, 200, r#"{"error":{"code":42}}"#);
        // Falls back to the stringified error object.
        assert!(r.error.unwrap().contains("42"));
    }

    #[test]
    fn parse_success_extracts_content_and_usage() {
        let body = r#"{"choices":[{"message":{"content":"hi there"}}],
                       "usage":{"prompt_tokens":12,"completion_tokens":34}}"#;
        let r = parse_model_response("m", 7, true, 200, body);
        assert!(r.error.is_none());
        assert_eq!(r.response, "hi there");
        assert_eq!(r.prompt_tokens, 12);
        assert_eq!(r.completion_tokens, 34);
        assert_eq!(r.duration_ms, 7);
    }

    #[test]
    fn parse_success_empty_content_falls_back() {
        let r = parse_model_response("m", 0, true, 200, r#"{"choices":[]}"#);
        assert_eq!(r.response, "(empty response)");
        assert_eq!(r.prompt_tokens, 0);
    }

    // --- run_survey fan-out: allSettled semantics (one failing model isolated) ---

    #[test]
    fn fanout_isolates_one_failing_model_allsettled() {
        // Model "bad/x" returns an error row; "good/y" returns a success row.
        // The panel keeps BOTH — one failure never aborts the others (R4).
        let call: Box<CallModel> = Box::new(|_key, model, _sys, _art| {
            if model == "bad/x" {
                ModelResult::failed(model, 3, "kaboom".to_string())
            } else {
                ModelResult {
                    model: model.to_string(),
                    response: "ok".to_string(),
                    duration_ms: 5,
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    error: None,
                }
            }
        });
        let ms = models(&["bad/x", "good/y"]);
        let results = run_survey("k", &ms, "sys", "art", &call, &|_| {});
        assert_eq!(results.len(), 2);
        // Order preserved (positional collection).
        assert_eq!(results[0].model, "bad/x");
        assert_eq!(results[0].error.as_deref(), Some("kaboom"));
        assert_eq!(results[1].model, "good/y");
        assert!(results[1].error.is_none());
    }

    #[test]
    fn fanout_panicking_model_becomes_failed_row_not_panic() {
        // A call that PANICS (the REJECTED-promise analogue) must not crash the
        // panel — its slot becomes a FAILED row.
        let call: Box<CallModel> = Box::new(|_key, model, _sys, _art| {
            if model == "panic/x" {
                panic!("model thread blew up");
            }
            ModelResult {
                model: model.to_string(),
                response: "ok".to_string(),
                duration_ms: 1,
                prompt_tokens: 0,
                completion_tokens: 0,
                error: None,
            }
        });
        let ms = models(&["panic/x", "fine/y"]);
        let results = run_survey("k", &ms, "s", "a", &call, &|_| {});
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].model, "panic/x");
        assert!(results[0].error.is_some(), "panicking model → failed row");
        assert!(results[1].error.is_none());
    }

    #[test]
    fn fanout_logs_per_model_progress() {
        let logged = Mutex::new(Vec::<String>::new());
        let call: Box<CallModel> = Box::new(|_k, model, _s, _a| ModelResult {
            model: model.to_string(),
            response: "ok".to_string(),
            duration_ms: 2,
            prompt_tokens: 0,
            completion_tokens: 0,
            error: None,
        });
        let ms = models(&["a/b"]);
        run_survey("k", &ms, "s", "the artifact", &call, &|m| {
            logged.lock().unwrap().push(m.to_string());
        });
        let lines = logged.into_inner().unwrap();
        // First line is the "Sending artifact (N chars) to M models..." header.
        assert!(lines[0].contains("Sending artifact"));
        assert!(lines[0].contains("1 models"));
        assert!(lines.iter().any(|l| l.contains("Done: a/b")));
    }

    // --- format_results byte-shape ---

    #[test]
    fn format_has_banners_headers_and_summary_table() {
        let results = vec![
            ModelResult {
                model: "anthropic/claude".to_string(),
                response: "great feedback".to_string(),
                duration_ms: 1500,
                prompt_tokens: 100,
                completion_tokens: 200,
                error: None,
            },
            ModelResult::failed("openai/gpt", 250, "HTTP 429: rate".to_string()),
        ];
        let out = format_results(
            &results,
            "be careful",
            &models(&["anthropic/claude", "openai/gpt"]),
        );
        assert!(out.contains(&"=".repeat(80)));
        assert!(out.contains("\nSURVEY\n"));
        assert!(out.contains("\nSUMMARY\n"));
        assert!(out.contains("Panel: anthropic/claude, openai/gpt"));
        assert!(out.contains("System prompt: be careful"));
        assert!(out.contains("MODEL: anthropic/claude"));
        assert!(out.contains("great feedback"));
        assert!(out.contains("[ERROR] HTTP 429: rate"));
        // Time formatting: >=1000ms → seconds with 1 decimal; <1000ms → ms.
        assert!(out.contains("1.5s"));
        assert!(out.contains("250ms"));
        // Status column.
        assert!(out.contains("OK"));
        assert!(out.contains("FAILED"));
    }

    #[test]
    fn format_name_width_floor_is_5() {
        // A short model name still gets a 5-wide Model column (Math.max(5, ...)).
        let results = vec![ModelResult {
            model: "a/b".to_string(),
            response: "x".to_string(),
            duration_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            error: None,
        }];
        let out = format_results(&results, "s", &models(&["a/b"]));
        // "Model" header (5 chars) padded to width 5; "a/b" (3) padded to 5.
        assert!(out.contains("a/b  "));
    }

    // --- real_call_model via ScriptedExec (curl seam — NO network, NO secret leak) ---

    #[test]
    fn real_call_model_success_through_curl_seam() {
        let tmp = tempfile::tempdir().unwrap();
        let body = r#"{"choices":[{"message":{"content":"seam ok"}}],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#;
        // curl emits the body then `-w` appends "\n200".
        let exec = ScriptedExec::new().on("curl", &["-sS"], Some(0), &format!("{body}\n200"), "");
        let call = real_call_model(&exec, tmp.path().to_str().unwrap());
        let r = call("sk-or-SECRETKEY", "a/b", "sys", "artifact text");
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.response, "seam ok");
        assert_eq!(r.prompt_tokens, 3);

        // G-S2: the secret rode the config stdin, never argv.
        let log = exec.log();
        let curl = log.iter().find(|i| i.cmd == "curl").unwrap();
        for tok in &curl.args {
            assert!(!tok.contains("SECRETKEY"), "secret in argv: {tok}");
        }
        assert!(curl
            .stdin
            .as_deref()
            .unwrap()
            .contains("Authorization: Bearer sk-or-SECRETKEY"));
    }

    #[test]
    fn real_call_model_http_error_becomes_failed_row() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = ScriptedExec::new().on("curl", &["-sS"], Some(0), "rate limited\n429", "");
        let call = real_call_model(&exec, tmp.path().to_str().unwrap());
        let r = call("k", "a/b", "s", "a");
        assert_eq!(r.error.as_deref(), Some("HTTP 429: rate limited"));
    }

    #[test]
    fn real_call_model_timeout_becomes_failed_row() {
        // A timed_out ExecResult → a FAILED row (allSettled discipline).
        let tmp = tempfile::tempdir().unwrap();
        let exec = TimeoutExec;
        let call = real_call_model(&exec, tmp.path().to_str().unwrap());
        let r = call("k", "a/b", "s", "a");
        assert_eq!(r.error.as_deref(), Some("request timed out"));
    }

    /// A tiny Exec double whose `run_with_stdin` reports a timeout.
    struct TimeoutExec;
    impl Exec for TimeoutExec {
        fn run(
            &self,
            _c: &str,
            _a: &[String],
            _e: &[(String, String)],
            _w: Option<&std::path::Path>,
            _t: Option<u64>,
        ) -> std::io::Result<crate::exec::ExecResult> {
            unreachable!()
        }
        fn run_with_stdin(
            &self,
            _c: &str,
            _a: &[String],
            _e: &[(String, String)],
            _w: Option<&std::path::Path>,
            _t: Option<u64>,
            _s: &[u8],
        ) -> std::io::Result<crate::exec::ExecResult> {
            Ok(crate::exec::ExecResult {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            })
        }
        fn spawn_inherit(
            &self,
            _c: &str,
            _a: &[String],
            _e: &[(String, String)],
            _w: Option<&std::path::Path>,
        ) -> std::io::Result<i32> {
            unreachable!()
        }
    }

    #[test]
    fn curl_available_probe_true_on_real_curl_or_seam() {
        let exec = ScriptedExec::new();
        // ScriptedExec.run is benign-success → invocable.
        assert!(curl_available(&exec));
    }

    #[test]
    fn split_trailing_status_parses_curl_w_output() {
        assert_eq!(split_trailing_status("body\n200"), ("body", 200));
        assert_eq!(
            split_trailing_status("multi\nline\nbody\n404"),
            ("multi\nline\nbody", 404)
        );
        // No status trailer → 0 (transport failure).
        assert_eq!(split_trailing_status("garbage").1, 0);
    }
}
