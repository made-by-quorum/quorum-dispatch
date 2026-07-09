//! S3 credential resolution (transcript-archive-spec.md "Ownership and
//! configuration": "the standard S3 credential chain applies (env vars →
//! `~/.aws/credentials` profile → instance roles in the cloud); config names
//! the profile, never the secret").
//!
//! **v1 scope note**: instance-role resolution (the third chain link, a
//! metadata-service query) is NOT implemented here. This box and the spec's
//! near-term targets are on-box garage + hand-provisioned AWS keys
//! ("Provisioning is manual per box for now"); a cloud-instance leg (plan.md
//! Atomic D) can extend [`resolve_credentials`] with that link later without
//! changing any caller — `None` still means "no credentials found," loud.

use crate::effects::Env;

#[derive(Debug, Clone, PartialEq)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
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
        None => {
            let home = env.var("HOME").filter(|s| !s.is_empty())?;
            format!("{home}/.aws/credentials")
        }
    };
    let text = std::fs::read_to_string(path).ok()?;
    parse_ini_profile(&text, profile.unwrap_or("default"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::collections::HashMap;

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        let mut vars = HashMap::new();
        for (k, v) in pairs {
            vars.insert(k.to_string(), v.to_string());
        }
        MapEnv { vars, uid: 501 }
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
        let creds =
            resolve_credentials(&env, Some("quorum-archive"), Some(path.to_str().unwrap()))
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
}
