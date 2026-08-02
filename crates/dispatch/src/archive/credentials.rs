//! S3 credential resolution (transcript-archive-spec.md "Ownership and
//! configuration": "the standard S3 credential chain applies (env vars →
//! `~/.aws/credentials` profile → instance roles in the cloud); config names
//! the profile, never the secret").
//!
//! Instance-role credentials use IMDSv2 as the third tier. The implementation
//! is hand-rolled over the archive HTTP client so tests can mock the HTTP
//! boundary and production does not pull in an AWS SDK.

use crate::effects::Env;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

#[derive(Debug)]
pub enum CredentialError {
    Unavailable(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Unavailable(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for CredentialError {}

#[derive(Debug, Clone)]
pub struct S3CredentialSource {
    inner: Arc<Mutex<S3CredentialSourceInner>>,
}

#[derive(Debug)]
enum S3CredentialSourceInner {
    Static(S3Credentials),
    Imds {
        endpoint: String,
        cached: Option<CachedImdsCredentials>,
    },
}

#[derive(Debug, Clone)]
struct CachedImdsCredentials {
    credentials: S3Credentials,
    expires_at_unix: i64,
}

const IMDS_DEFAULT_ENDPOINT: &str = "http://169.254.169.254";
const IMDS_REFRESH_SKEW_SECONDS: i64 = 300;

impl S3CredentialSource {
    pub fn static_credentials(credentials: S3Credentials) -> Self {
        Self {
            inner: Arc::new(Mutex::new(S3CredentialSourceInner::Static(credentials))),
        }
    }

    pub fn imds(endpoint: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(S3CredentialSourceInner::Imds {
                endpoint: endpoint.into(),
                cached: None,
            })),
        }
    }

    pub fn credentials(&self) -> Result<S3Credentials, CredentialError> {
        self.credentials_at(now_unix_seconds())
    }

    pub fn credentials_at(&self, now_unix: i64) -> Result<S3Credentials, CredentialError> {
        let mut inner = self.inner.lock().map_err(|_| {
            CredentialError::Unavailable("S3 credential cache lock poisoned".to_string())
        })?;
        match &mut *inner {
            S3CredentialSourceInner::Static(c) => Ok(c.clone()),
            S3CredentialSourceInner::Imds { endpoint, cached } => {
                if let Some(c) = cached {
                    if now_unix < c.expires_at_unix - IMDS_REFRESH_SKEW_SECONDS {
                        return Ok(c.credentials.clone());
                    }
                }
                let next = imdsv2_credentials(endpoint)?;
                let out = next.credentials.clone();
                *cached = Some(next);
                Ok(out)
            }
        }
    }
}

/// Resolve credentials via the standard chain's first two links: env vars,
/// then a named profile in an INI-shaped credentials file (real production:
/// `~/.aws/credentials`, resolved from `env.var("HOME")`; `credentials_file`
/// lets tests point at a fixture instead of touching `$HOME`).
///
/// `profile` is `[archive] credentials_profile` from dispatch's config;
/// `None` falls back to the AWS CLI convention profile name `"default"`.
pub fn resolve_credentials(
    env: &dyn Env,
    profile: Option<&str>,
    credentials_file: Option<&str>,
) -> Option<S3Credentials> {
    if let (Some(key), Some(secret)) = (
        env.var("AWS_ACCESS_KEY_ID"),
        env.var("AWS_SECRET_ACCESS_KEY"),
    ) {
        if !key.is_empty() && !secret.is_empty() {
            return Some(S3Credentials {
                access_key_id: key,
                secret_access_key: secret,
                session_token: env.var("AWS_SESSION_TOKEN").filter(|s| !s.is_empty()),
            });
        }
    }

    let path = match credentials_file {
        Some(p) => p.to_string(),
        None => match env.var("HOME").filter(|s| !s.is_empty()) {
            Some(home) => format!("{home}/.aws/credentials"),
            None => {
                return imdsv2_credentials(&imds_endpoint_from_env(env))
                    .ok()
                    .map(|c| c.credentials)
            }
        },
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) if credentials_file.is_none() || env.var("QBT_TEST_IMDS_ENDPOINT").is_some() => {
            return imdsv2_credentials(&imds_endpoint_from_env(env))
                .ok()
                .map(|c| c.credentials)
        }
        Err(_) => return None,
    };
    if let Some(creds) = parse_ini_profile(&text, profile.unwrap_or("default")) {
        return Some(creds);
    }
    if credentials_file.is_some() && env.var("QBT_TEST_IMDS_ENDPOINT").is_none() {
        return None;
    }

    imdsv2_credentials(&imds_endpoint_from_env(env))
        .ok()
        .map(|c| c.credentials)
}

pub fn resolve_credential_source(
    env: &dyn Env,
    profile: Option<&str>,
    credentials_file: Option<&str>,
) -> Option<S3CredentialSource> {
    if let (Some(key), Some(secret)) = (
        env.var("AWS_ACCESS_KEY_ID"),
        env.var("AWS_SECRET_ACCESS_KEY"),
    ) {
        if !key.is_empty() && !secret.is_empty() {
            return Some(S3CredentialSource::static_credentials(S3Credentials {
                access_key_id: key,
                secret_access_key: secret,
                session_token: env.var("AWS_SESSION_TOKEN").filter(|s| !s.is_empty()),
            }));
        }
    }

    let path = match credentials_file {
        Some(p) => p.to_string(),
        None => match env.var("HOME").filter(|s| !s.is_empty()) {
            Some(home) => format!("{home}/.aws/credentials"),
            None => return Some(S3CredentialSource::imds(imds_endpoint_from_env(env))),
        },
    };
    let text = std::fs::read_to_string(path).ok();
    if let Some(creds) = text
        .as_deref()
        .and_then(|text| parse_ini_profile(text, profile.unwrap_or("default")))
    {
        return Some(S3CredentialSource::static_credentials(creds));
    }
    if credentials_file.is_some() && env.var("QBT_TEST_IMDS_ENDPOINT").is_none() {
        return None;
    }

    Some(S3CredentialSource::imds(imds_endpoint_from_env(env)))
}

/// Parse one `[profile]` section out of an INI-shaped credentials file.
/// Permissive: unknown keys are ignored; a missing profile, or a profile
/// missing either required key, yields `None` rather than a partial value.
pub fn parse_ini_profile(text: &str, profile: &str) -> Option<S3Credentials> {
    let mut in_profile = false;
    let mut access_key_id = None;
    let mut secret_access_key = None;
    let mut session_token = None;
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_profile = name.trim() == profile;
            continue;
        }
        if !in_profile {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').to_string();
            match k {
                "aws_access_key_id" => access_key_id = Some(v),
                "aws_secret_access_key" => secret_access_key = Some(v),
                "aws_session_token" => session_token = Some(v),
                _ => {}
            }
        }
    }
    Some(S3Credentials {
        access_key_id: access_key_id?,
        secret_access_key: secret_access_key?,
        session_token,
    })
}

fn imds_endpoint_from_env(env: &dyn Env) -> String {
    env.var("QBT_TEST_IMDS_ENDPOINT")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| IMDS_DEFAULT_ENDPOINT.to_string())
}

fn imdsv2_credentials(endpoint: &str) -> Result<CachedImdsCredentials, CredentialError> {
    let ep = Endpoint::parse(endpoint)?;
    let token_resp = http_request(
        &ep,
        "PUT",
        "/latest/api/token",
        &[(
            "x-aws-ec2-metadata-token-ttl-seconds".to_string(),
            "21600".to_string(),
        )],
        b"",
    )?;
    if token_resp.status != 200 {
        return Err(CredentialError::Unavailable(format!(
            "IMDSv2 token status {}",
            token_resp.status
        )));
    }
    let token = String::from_utf8(token_resp.body)
        .map_err(|e| CredentialError::Unavailable(format!("IMDSv2 token not utf-8: {e}")))?;
    let token_header = [("x-aws-ec2-metadata-token".to_string(), token)];

    let role_resp = http_request(
        &ep,
        "GET",
        "/latest/meta-data/iam/security-credentials/",
        &token_header,
        b"",
    )?;
    if role_resp.status != 200 {
        return Err(CredentialError::Unavailable(format!(
            "IMDS role-name status {}",
            role_resp.status
        )));
    }
    let role = String::from_utf8(role_resp.body)
        .map_err(|e| CredentialError::Unavailable(format!("IMDS role-name not utf-8: {e}")))?;
    let role = role.lines().next().unwrap_or("").trim();
    if role.is_empty() {
        return Err(CredentialError::Unavailable(
            "IMDS returned no role name".to_string(),
        ));
    }

    let cred_resp = http_request(
        &ep,
        "GET",
        &format!("/latest/meta-data/iam/security-credentials/{role}"),
        &token_header,
        b"",
    )?;
    if cred_resp.status != 200 {
        return Err(CredentialError::Unavailable(format!(
            "IMDS credentials status {}",
            cred_resp.status
        )));
    }
    let v: serde_json::Value = serde_json::from_slice(&cred_resp.body)
        .map_err(|e| CredentialError::Unavailable(format!("IMDS credentials JSON: {e}")))?;
    let expires_at = json_str(&v, "Expiration")?;
    Ok(CachedImdsCredentials {
        credentials: S3Credentials {
            access_key_id: json_str(&v, "AccessKeyId")?.to_string(),
            secret_access_key: json_str(&v, "SecretAccessKey")?.to_string(),
            session_token: Some(json_str(&v, "Token")?.to_string()),
        },
        expires_at_unix: parse_rfc3339_z(expires_at).ok_or_else(|| {
            CredentialError::Unavailable(format!(
                "IMDS Expiration is not RFC3339 UTC: {expires_at}"
            ))
        })?,
    })
}

struct Endpoint {
    authority: String,
    tls_server_name: Option<String>,
}

impl Endpoint {
    fn parse(endpoint: &str) -> Result<Self, CredentialError> {
        let (scheme, rest) = endpoint.split_once("://").ok_or_else(|| {
            CredentialError::Unavailable(format!("IMDS endpoint lacks scheme: {endpoint}"))
        })?;
        let authority = rest.trim_end_matches('/').to_string();
        if authority.is_empty() || authority.contains('/') {
            return Err(CredentialError::Unavailable(format!(
                "malformed IMDS endpoint: {endpoint}"
            )));
        }
        let tls_server_name = match scheme {
            "http" => None,
            "https" => Some(
                authority
                    .split(':')
                    .next()
                    .unwrap_or(&authority)
                    .to_string(),
            ),
            _ => {
                return Err(CredentialError::Unavailable(format!(
                    "unsupported IMDS endpoint scheme: {scheme}"
                )))
            }
        };
        Ok(Self {
            authority,
            tls_server_name,
        })
    }
}

fn http_request(
    ep: &Endpoint,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<crate::archive::http::HttpResponse, CredentialError> {
    crate::archive::http::request_on(
        &ep.authority,
        ep.tls_server_name.as_deref(),
        method,
        path,
        headers,
        body,
    )
    .map_err(|e| CredentialError::Unavailable(e.to_string()))
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str, CredentialError> {
    v.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        CredentialError::Unavailable(format!("IMDS JSON missing string field {key}"))
    })
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_rfc3339_z(s: &str) -> Option<i64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.split('-');
    let y: i32 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let hh: u32 = t.next()?.parse().ok()?;
    let mm: u32 = t.next()?.parse().ok()?;
    let ss_raw = t.next()?;
    if t.next().is_some() {
        return None;
    }
    let ss: u32 = ss_raw.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, m, day);
    Some(days * 86_400 + (hh as i64) * 3600 + (mm as i64) * 60 + ss as i64)
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m as i32 + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        let mut vars = HashMap::new();
        for (k, v) in pairs {
            vars.insert(k.to_string(), v.to_string());
        }
        MapEnv { vars, uid: 501 }
    }

    struct MockImds {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MockImds {
        fn start(credential_bodies: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let handle = thread::spawn(move || {
                let mut cred_iter = credential_bodies.into_iter();
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let mut headers = Vec::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        headers.push(line.trim_end().to_string());
                    }
                    let record = format!(
                        "{}{}",
                        first.trim_end(),
                        if headers.is_empty() {
                            String::new()
                        } else {
                            format!("\n{}", headers.join("\n"))
                        }
                    );
                    seen.lock().unwrap().push(record);
                    let path = first.split_whitespace().nth(1).unwrap_or("");
                    let (status, body) = match path {
                        "/latest/api/token" => ("200 OK", "tok".to_string()),
                        "/latest/meta-data/iam/security-credentials/" => {
                            ("200 OK", "role-a\n".to_string())
                        }
                        "/latest/meta-data/iam/security-credentials/role-a" => (
                            "200 OK",
                            cred_iter.next().unwrap_or_else(|| "{}".to_string()),
                        ),
                        _ => ("404 Not Found", "missing".to_string()),
                    };
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            Self {
                endpoint,
                requests,
                handle: Some(handle),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MockImds {
        fn drop(&mut self) {
            let _ = self.handle.take();
        }
    }

    fn imds_body(access_key: &str, secret: &str, token: &str, expiration: &str) -> String {
        format!(
            "{{\"AccessKeyId\":\"{access_key}\",\"SecretAccessKey\":\"{secret}\",\"Token\":\"{token}\",\"Expiration\":\"{expiration}\"}}"
        )
    }

    #[test]
    fn env_vars_win_when_present() {
        let env = map_env(&[
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "secretenv"),
        ]);
        let creds = resolve_credentials(&env, None, Some("/nonexistent")).unwrap();
        assert_eq!(creds.access_key_id, "AKIDENV");
        assert_eq!(creds.secret_access_key, "secretenv");
        assert_eq!(creds.session_token, None);
    }

    #[test]
    fn env_session_token_carried_when_present() {
        let env = map_env(&[
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "secretenv"),
            ("AWS_SESSION_TOKEN", "tok"),
        ]);
        let creds = resolve_credentials(&env, None, Some("/nonexistent")).unwrap();
        assert_eq!(creds.session_token.as_deref(), Some("tok"));
    }

    #[test]
    fn falls_through_to_credentials_file_profile_when_env_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        std::fs::write(
            &path,
            "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = secretdefault\n\n\
             [quorum-archive]\naws_access_key_id = AKIDPROFILE\naws_secret_access_key = secretprofile\n",
        )
        .unwrap();
        let env = map_env(&[]);
        let creds = resolve_credentials(&env, Some("quorum-archive"), Some(path.to_str().unwrap()))
            .unwrap();
        assert_eq!(creds.access_key_id, "AKIDPROFILE");
        assert_eq!(creds.secret_access_key, "secretprofile");

        // No profile named -> "default".
        let creds = resolve_credentials(&env, None, Some(path.to_str().unwrap())).unwrap();
        assert_eq!(creds.access_key_id, "AKIDDEFAULT");
    }

    #[test]
    fn missing_file_and_missing_profile_yield_none() {
        let env = map_env(&[]);
        assert_eq!(
            resolve_credentials(&env, Some("x"), Some("/definitely/not/here")),
            None
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        std::fs::write(&path, "[default]\naws_access_key_id = a\n").unwrap(); // missing secret
        assert_eq!(
            resolve_credentials(&env, None, Some(path.to_str().unwrap())),
            None
        );
    }

    #[test]
    fn empty_env_values_do_not_shadow_the_file_tier() {
        // An exported-but-empty env var (a common shell artifact) must not
        // win over a real file-tier credential.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        std::fs::write(
            &path,
            "[default]\naws_access_key_id = AKIDFILE\naws_secret_access_key = secretfile\n",
        )
        .unwrap();
        let env = map_env(&[("AWS_ACCESS_KEY_ID", ""), ("AWS_SECRET_ACCESS_KEY", "")]);
        let creds = resolve_credentials(&env, None, Some(path.to_str().unwrap())).unwrap();
        assert_eq!(creds.access_key_id, "AKIDFILE");
    }

    #[test]
    fn env_present_never_contacts_imdsv2() {
        let imds = MockImds::start(vec![]);
        let env = map_env(&[
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "secretenv"),
            ("QBT_TEST_IMDS_ENDPOINT", &imds.endpoint),
        ]);
        let source = resolve_credential_source(&env, None, Some("/nonexistent")).unwrap();
        let creds = source.credentials().unwrap();
        assert_eq!(creds.access_key_id, "AKIDENV");
        assert_eq!(imds.request_count(), 0);
    }

    #[test]
    fn env_and_file_absent_fetches_imdsv2_credentials() {
        let first = imds_body(
            "AKIDIMDS",
            "secretimds",
            "sessiontok",
            "2099-01-01T00:00:00Z",
        );
        let imds = MockImds::start(vec![first]);
        let env = map_env(&[("QBT_TEST_IMDS_ENDPOINT", &imds.endpoint)]);
        let source = resolve_credential_source(&env, None, Some("/nonexistent")).unwrap();
        let creds = source.credentials().unwrap();
        assert_eq!(creds.access_key_id, "AKIDIMDS");
        assert_eq!(creds.secret_access_key, "secretimds");
        assert_eq!(creds.session_token.as_deref(), Some("sessiontok"));
        let requests = imds.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert!(requests[0].starts_with("PUT /latest/api/token HTTP/1.1"));
        assert!(requests[0].contains("x-aws-ec2-metadata-token-ttl-seconds: 21600"));
        assert!(requests[1].starts_with("GET /latest/meta-data/iam/security-credentials/ HTTP/1.1"));
        assert!(requests[1].contains("x-aws-ec2-metadata-token: tok"));
        assert!(requests[2]
            .starts_with("GET /latest/meta-data/iam/security-credentials/role-a HTTP/1.1"));
        assert!(requests[2].contains("x-aws-ec2-metadata-token: tok"));
    }

    #[test]
    fn imdsv2_expiry_rollover_refreshes_and_uses_new_credentials() {
        let first = imds_body("AKIDOLD", "secretold", "tokold", "2026-08-02T00:10:00Z");
        let second = imds_body("AKIDNEW", "secretnew", "toknew", "2099-01-01T00:00:00Z");
        let imds = MockImds::start(vec![first, second]);
        let source = S3CredentialSource::imds(&imds.endpoint);
        let old = source
            .credentials_at(parse_rfc3339_z("2026-08-02T00:00:00Z").unwrap())
            .unwrap();
        assert_eq!(old.access_key_id, "AKIDOLD");
        let cached = source
            .credentials_at(parse_rfc3339_z("2026-08-02T00:03:00Z").unwrap())
            .unwrap();
        assert_eq!(cached.access_key_id, "AKIDOLD");
        let new = source
            .credentials_at(parse_rfc3339_z("2026-08-02T00:06:00Z").unwrap())
            .unwrap();
        assert_eq!(new.access_key_id, "AKIDNEW");
        assert_eq!(new.session_token.as_deref(), Some("toknew"));
        assert_eq!(imds.requests().len(), 6);
    }
}
