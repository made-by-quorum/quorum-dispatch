//! Minimal S3-compatible object client (GET/PUT only) — SigV4 signing
//! (`sigv4.rs`) over the hand-rolled HTTP/1.1 transport (`http.rs`). See
//! `http.rs`'s module doc for why this is hand-rolled.

use crate::archive::credentials::S3Credentials;
use crate::archive::http::{self, HttpError};
use crate::archive::sigv4::{self, SignParams};

#[derive(Debug)]
pub enum S3Error {
    /// Transport failure — could not reach the endpoint at all.
    Unreachable(String),
    /// The configured endpoint uses a scheme this client cannot speak.
    UnsupportedScheme(String),
    /// The store responded with a non-2xx/404 status.
    Http { status: u16, body: String },
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3Error::Unreachable(e) => write!(f, "S3 store unreachable: {e}"),
            S3Error::UnsupportedScheme(s) => write!(
                f,
                "unsupported archive endpoint scheme {s:?}: this build only speaks plaintext \
                 HTTP (see archive/http.rs's module doc) — an https:// endpoint needs Atomic D's \
                 TLS wiring"
            ),
            S3Error::Http { status, body } => write!(f, "S3 request failed: HTTP {status} {body}"),
        }
    }
}
impl std::error::Error for S3Error {}

impl From<HttpError> for S3Error {
    fn from(e: HttpError) -> Self {
        S3Error::Unreachable(e.0)
    }
}

/// The GET/PUT seam a prefix-check-then-copy algorithm runs against (see
/// frame's `persist.rs` for the transcript-copy caller, and this file's own
/// tests for the divergence/overwrite/written cases exercised against
/// [`FakeStore`]-shaped fixtures). [`S3Client`] is the real (network)
/// implementation; callers substitute an in-memory fake so prefix-check/
/// divergence LOGIC is verified without a real socket — the wire format
/// itself is `http.rs`'s test responsibility.
pub trait ObjectStore {
    fn get_object(&self, key: &str, now: &str) -> Result<Option<Vec<u8>>, S3Error>;
    fn put_object(&self, key: &str, body: &[u8], now: &str) -> Result<(), S3Error>;
}

impl ObjectStore for S3Client {
    fn get_object(&self, key: &str, now: &str) -> Result<Option<Vec<u8>>, S3Error> {
        S3Client::get_object(self, key, now)
    }
    fn put_object(&self, key: &str, body: &[u8], now: &str) -> Result<(), S3Error> {
        S3Client::put_object(self, key, body, now)
    }
}

#[derive(Debug)]
pub struct S3Client {
    /// `host:port` — what `TcpStream::connect` and the signed `Host` header
    /// both use.
    authority: String,
    bucket: String,
    region: String,
    credentials: S3Credentials,
}

impl S3Client {
    /// Plain fields, not a config type: this client is a generic S3-
    /// compatible object store adapter, deliberately decoupled from any one
    /// crate's `[archive]` TOML shape — frame and dispatch each own their own
    /// config surface and both construct a client from their own resolved
    /// values (persist-relocation: dispatch's `ArchiveConfig`/`[archive]`
    /// section is gone entirely; frame's `FrameArchiveConfig`/`FramePersistConfig`
    /// pass their fields here directly, no shim struct in between).
    pub fn new(
        endpoint: &str,
        bucket: &str,
        region: &str,
        credentials: S3Credentials,
    ) -> Result<Self, S3Error> {
        let (scheme, authority) = split_endpoint(endpoint)
            .ok_or_else(|| S3Error::Unreachable(format!("malformed endpoint {endpoint:?}")))?;
        if scheme != "http" {
            return Err(S3Error::UnsupportedScheme(scheme.to_string()));
        }
        let authority = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        Ok(S3Client {
            authority,
            bucket: bucket.to_string(),
            region: region.to_string(),
            credentials,
        })
    }

    /// GET `<bucket>/<key>`. `Ok(None)` on a clean 404 — object absent, the
    /// ordinary "no prior copy" case, never an error. Anything else
    /// non-2xx, or a transport failure, is `Err` (loud, per the no-silent-
    /// gaps standard).
    pub fn get_object(&self, key: &str, now: &str) -> Result<Option<Vec<u8>>, S3Error> {
        let path = sigv4::uri_encode_path(&format!("/{}/{key}", self.bucket));
        let headers = self.sign("GET", &path, &[], now);
        let resp = http::request(&self.authority, "GET", &path, &headers, &[])?;
        match resp.status {
            404 => Ok(None),
            200..=299 => Ok(Some(resp.body)),
            status => Err(S3Error::Http {
                status,
                body: String::from_utf8_lossy(&resp.body).into_owned(),
            }),
        }
    }

    /// PUT `<bucket>/<key>` with `body` copied verbatim — never transformed,
    /// wrapped, or re-encoded (spec: "Transcript layout").
    pub fn put_object(&self, key: &str, body: &[u8], now: &str) -> Result<(), S3Error> {
        let path = sigv4::uri_encode_path(&format!("/{}/{key}", self.bucket));
        let headers = self.sign("PUT", &path, body, now);
        let resp = http::request(&self.authority, "PUT", &path, &headers, body)?;
        match resp.status {
            200..=299 => Ok(()),
            status => Err(S3Error::Http {
                status,
                body: String::from_utf8_lossy(&resp.body).into_owned(),
            }),
        }
    }

    fn sign(&self, method: &str, encoded_path: &str, body: &[u8], now: &str) -> Vec<(String, String)> {
        let signed = sigv4::sign(&SignParams {
            method,
            host: &self.authority,
            path: encoded_path,
            region: &self.region,
            access_key_id: &self.credentials.access_key_id,
            secret_access_key: &self.credentials.secret_access_key,
            session_token: self.credentials.session_token.as_deref(),
            payload: body,
            amz_date: now,
        });
        let mut headers = vec![
            ("host".to_string(), self.authority.clone()),
            ("x-amz-date".to_string(), signed.x_amz_date),
            (
                "x-amz-content-sha256".to_string(),
                signed.x_amz_content_sha256,
            ),
            ("authorization".to_string(), signed.authorization),
        ];
        if let Some(token) = signed.x_amz_security_token {
            headers.push(("x-amz-security-token".to_string(), token));
        }
        headers
    }
}

/// Split `"http://host:port"` into `("http", "host:port")`. Authority-only —
/// a path/query on the configured endpoint is rejected by the empty check
/// below firing on garbage input rather than silently dropped.
fn split_endpoint(endpoint: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() || authority.contains('/') {
        return None;
    }
    Some((scheme, authority))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> S3Credentials {
        S3Credentials {
            access_key_id: "AKID".to_string(),
            secret_access_key: "secret".to_string(),
            session_token: None,
        }
    }

    #[test]
    fn https_endpoint_is_rejected_loudly_not_silently_tried_as_http() {
        let err =
            S3Client::new("https://s3.amazonaws.com", "b", "us-east-1", creds()).unwrap_err();
        assert!(matches!(err, S3Error::UnsupportedScheme(s) if s == "https"));
    }

    #[test]
    fn http_endpoint_without_port_defaults_to_80() {
        let client = S3Client::new("http://example.com", "b", "us-east-1", creds()).unwrap();
        assert_eq!(client.authority, "example.com:80");
    }

    #[test]
    fn http_endpoint_with_port_preserved() {
        let client = S3Client::new("http://127.0.0.1:3900", "b", "us-east-1", creds()).unwrap();
        assert_eq!(client.authority, "127.0.0.1:3900");
    }

    #[test]
    fn trailing_slash_on_endpoint_is_tolerated() {
        let client = S3Client::new("http://127.0.0.1:3900/", "b", "us-east-1", creds()).unwrap();
        assert_eq!(client.authority, "127.0.0.1:3900");
    }

    #[test]
    fn malformed_endpoint_is_rejected() {
        assert!(S3Client::new("not-a-url", "b", "us-east-1", creds()).is_err());
        assert!(S3Client::new("http://", "b", "us-east-1", creds()).is_err());
    }

    // End-to-end GET/PUT wiring against a real TcpListener lives in
    // backup.rs's tests (a fake S3-shaped server exercising the full
    // prefix-check flow) — kept there so the fixture is built once and
    // shared across the divergence/overwrite/written cases.
}
