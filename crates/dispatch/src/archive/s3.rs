//! Minimal S3-compatible object client (GET/PUT only) — SigV4 signing
//! (`sigv4.rs`) over the hand-rolled HTTP/1.1 transport (`http.rs`). See
//! `http.rs`'s module doc for why this is hand-rolled.

use crate::archive::credentials::{S3CredentialSource, S3Credentials};
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
                "unsupported S3 endpoint scheme {s:?}: plaintext http:// is refused; only https:// \
                 (rustls, see archive/http.rs's module doc) is supported"
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
    /// Cheap existence probe (HTTP HEAD): `Ok(true)` if the object is present,
    /// `Ok(false)` on a clean 404. Used by the refs-first blob upload
    /// (frame `archive_hook`) to skip re-PUTting a LARGE already-present,
    /// content-addressed blob — the round trip pays for itself only above a
    /// size threshold (below it, an unconditional idempotent PUT is cheaper
    /// than a HEAD+PUT).
    ///
    /// Default: conservatively report absent so the caller PUTs. Because blobs
    /// are content-addressed, a false "absent" only ever costs a redundant,
    /// byte-identical PUT — never a correctness or ordering violation — so an
    /// in-memory fake that does not model HEAD stays safe by inheriting this.
    /// The real network client ([`S3Client`]) overrides it.
    fn head_object(&self, _key: &str, _now: &str) -> Result<bool, S3Error> {
        Ok(false)
    }
}

impl ObjectStore for S3Client {
    fn get_object(&self, key: &str, now: &str) -> Result<Option<Vec<u8>>, S3Error> {
        S3Client::get_object(self, key, now)
    }
    fn put_object(&self, key: &str, body: &[u8], now: &str) -> Result<(), S3Error> {
        S3Client::put_object(self, key, body, now)
    }
    fn head_object(&self, key: &str, now: &str) -> Result<bool, S3Error> {
        S3Client::head_object(self, key, now)
    }
}

#[derive(Debug)]
pub struct S3Client {
    /// What the signed `Host` header carries: `host:port` for http (garage
    /// convention, port always explicit) and for https-with-explicit-port;
    /// bare `host` for https on the default port 443 (AWS SigV4 convention —
    /// the sent and signed Host must simply match, and AWS's own SDKs omit
    /// the default port).
    authority: String,
    /// What `TcpStream::connect` dials — always `host:port`.
    connect_addr: String,
    /// `Some(hostname)` iff the endpoint is https — the name the server's
    /// certificate must match (SNI + verification), threaded to
    /// [`http::request_on`].
    tls_server_name: Option<String>,
    bucket: String,
    region: String,
    credentials: S3CredentialSource,
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
        Self::new_with_credential_source(
            endpoint,
            bucket,
            region,
            S3CredentialSource::static_credentials(credentials),
        )
    }

    pub fn new_with_credential_source(
        endpoint: &str,
        bucket: &str,
        region: &str,
        credentials: S3CredentialSource,
    ) -> Result<Self, S3Error> {
        let (scheme, authority) = split_endpoint(endpoint)
            .ok_or_else(|| S3Error::Unreachable(format!("malformed endpoint {endpoint:?}")))?;
        let (authority, connect_addr, tls_server_name) = match scheme {
            "http" => return Err(S3Error::UnsupportedScheme("http".to_string())),
            "https" => match authority.split_once(':') {
                Some((host, _port)) => (
                    authority.to_string(),
                    authority.to_string(),
                    Some(host.to_string()),
                ),
                None => (
                    authority.to_string(),
                    format!("{authority}:443"),
                    Some(authority.to_string()),
                ),
            },
            other => return Err(S3Error::UnsupportedScheme(other.to_string())),
        };
        Ok(S3Client {
            authority,
            connect_addr,
            tls_server_name,
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
        let headers = self.sign("GET", &path, &[], now)?;
        let resp = http::request_on(
            &self.connect_addr,
            self.tls_server_name.as_deref(),
            "GET",
            &path,
            &headers,
            &[],
        )?;
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
        let headers = self.sign("PUT", &path, body, now)?;
        let resp = http::request_on(
            &self.connect_addr,
            self.tls_server_name.as_deref(),
            "PUT",
            &path,
            &headers,
            body,
        )?;
        match resp.status {
            200..=299 => Ok(()),
            status => Err(S3Error::Http {
                status,
                body: String::from_utf8_lossy(&resp.body).into_owned(),
            }),
        }
    }

    /// HEAD `<bucket>/<key>` — existence only, no body transferred. `Ok(true)`
    /// on a 2xx, `Ok(false)` on a clean 404, `Err` on anything else or a
    /// transport failure (loud, per the no-silent-gaps standard — same
    /// disposition as `get_object`). The signed payload is empty exactly as a
    /// GET's is; `http::request` discards any `Content-Length`-announced body
    /// on a HEAD response (RFC 9110 §9.3.2 — a HEAD response never carries
    /// one), so this never blocks reading a body the server did not send.
    pub fn head_object(&self, key: &str, now: &str) -> Result<bool, S3Error> {
        let path = sigv4::uri_encode_path(&format!("/{}/{key}", self.bucket));
        let headers = self.sign("HEAD", &path, &[], now)?;
        let resp = http::request_on(
            &self.connect_addr,
            self.tls_server_name.as_deref(),
            "HEAD",
            &path,
            &headers,
            &[],
        )?;
        match resp.status {
            404 => Ok(false),
            200..=299 => Ok(true),
            status => Err(S3Error::Http {
                status,
                body: String::from_utf8_lossy(&resp.body).into_owned(),
            }),
        }
    }

    /// SigV4 requires the request's `x-amz-date` to be within ~15 minutes of
    /// server time (AWS rejects with `RequestTimeTooSkewed` beyond that; garage
    /// never checks, which kept this dormant). Callers historically threaded a
    /// `now` captured ONCE at the top of a long walk — every persist signed
    /// after minute 15 of a walk then failed against AWS, and end-of-walk
    /// projection PUTs failed on every long walk. The real client therefore
    /// stamps EACH request at sign time and ignores the caller's `now`; the
    /// parameter stays on the [`ObjectStore`] surface for fakes and call-site
    /// compatibility.
    fn sign(
        &self,
        method: &str,
        encoded_path: &str,
        body: &[u8],
        _caller_now: &str,
    ) -> Result<Vec<(String, String)>, S3Error> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let now = quorum_core::timefmt::epoch_ms_to_amz_date(now_ms);
        let now = now.as_str();
        let credentials = self
            .credentials
            .credentials()
            .map_err(|e| S3Error::Unreachable(format!("S3 credentials: {e}")))?;
        let signed = sigv4::sign(&SignParams {
            method,
            host: &self.authority,
            path: encoded_path,
            region: &self.region,
            access_key_id: &credentials.access_key_id,
            secret_access_key: &credentials.secret_access_key,
            session_token: credentials.session_token.as_deref(),
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
        Ok(headers)
    }
}

/// Split `"scheme://host:port"` into `("scheme", "host:port")`. Authority-only —
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
    fn https_endpoint_default_port_signs_bare_host_and_dials_443() {
        let client = S3Client::new(
            "https://s3.us-east-2.amazonaws.com",
            "b",
            "us-east-2",
            creds(),
        )
        .unwrap();
        // AWS SigV4 convention: the Host header (== signed authority) omits
        // the default port; the socket still dials :443; the certificate must
        // match the bare hostname.
        assert_eq!(client.authority, "s3.us-east-2.amazonaws.com");
        assert_eq!(client.connect_addr, "s3.us-east-2.amazonaws.com:443");
        assert_eq!(
            client.tls_server_name.as_deref(),
            Some("s3.us-east-2.amazonaws.com")
        );
    }

    #[test]
    fn https_endpoint_with_explicit_port_keeps_it_in_host_and_dial() {
        let client = S3Client::new("https://minio.local:9443", "b", "us-east-1", creds()).unwrap();
        assert_eq!(client.authority, "minio.local:9443");
        assert_eq!(client.connect_addr, "minio.local:9443");
        assert_eq!(client.tls_server_name.as_deref(), Some("minio.local"));
    }

    #[test]
    fn http_endpoint_is_refused_before_requests() {
        let err = S3Client::new("http://127.0.0.1:3900", "b", "us-east-1", creds()).unwrap_err();
        assert!(matches!(&err, S3Error::UnsupportedScheme(s) if s == "http"));
        assert!(err.to_string().contains("plaintext"), "{err}");
    }

    #[test]
    fn sign_stamps_fresh_time_ignoring_callers_stale_now() {
        // The RequestTimeTooSkewed fix: a caller threading a walk-start `now`
        // (here: epoch) must NOT control the signed x-amz-date — the client
        // stamps at sign time, keeping every request inside AWS's 15-min skew
        // window no matter how long the calling walk runs.
        let client = S3Client::new(
            "https://s3.us-east-2.amazonaws.com",
            "b",
            "us-east-2",
            creds(),
        )
        .unwrap();
        let headers = client
            .sign("PUT", "/b/k", b"x", "19700101T000000Z")
            .unwrap();
        let amz_date = &headers.iter().find(|(k, _)| k == "x-amz-date").unwrap().1;
        assert_ne!(
            amz_date, "19700101T000000Z",
            "stale caller time must be ignored"
        );
        assert!(
            amz_date.len() == 16 && amz_date.ends_with('Z') && amz_date.contains('T'),
            "fresh YYYYMMDD'T'HHMMSS'Z' stamp, got {amz_date:?}"
        );
    }

    #[test]
    fn sign_includes_security_token_for_temporary_credentials() {
        let client = S3Client::new(
            "https://s3.us-east-2.amazonaws.com",
            "b",
            "us-east-2",
            S3Credentials {
                access_key_id: "AKID".to_string(),
                secret_access_key: "secret".to_string(),
                session_token: Some("tok".to_string()),
            },
        )
        .unwrap();
        let headers = client.sign("GET", "/b/k", &[], "ignored").unwrap();
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-amz-security-token")
                .map(|(_, v)| v.as_str()),
            Some("tok")
        );
        assert!(headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1
            .contains("x-amz-security-token"));
    }

    #[test]
    fn sign_omits_security_token_for_static_credentials_without_token() {
        let client = S3Client::new(
            "https://s3.us-east-2.amazonaws.com",
            "b",
            "us-east-2",
            creds(),
        )
        .unwrap();
        let headers = client.sign("GET", "/b/k", &[], "ignored").unwrap();
        assert!(headers.iter().all(|(k, _)| k != "x-amz-security-token"));
        assert!(!headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1
            .contains("x-amz-security-token"));
    }

    #[test]
    fn non_http_schemes_are_rejected_loudly() {
        let err = S3Client::new("ftp://example.com", "b", "us-east-1", creds()).unwrap_err();
        assert!(matches!(err, S3Error::UnsupportedScheme(s) if s == "ftp"));
    }

    #[test]
    fn http_endpoint_without_port_is_refused() {
        let err = S3Client::new("http://example.com", "b", "us-east-1", creds()).unwrap_err();
        assert!(matches!(err, S3Error::UnsupportedScheme(s) if s == "http"));
    }

    #[test]
    fn http_endpoint_with_port_is_refused() {
        let err = S3Client::new("http://127.0.0.1:3900", "b", "us-east-1", creds()).unwrap_err();
        assert!(matches!(err, S3Error::UnsupportedScheme(s) if s == "http"));
    }

    #[test]
    fn trailing_slash_on_endpoint_is_tolerated() {
        let client = S3Client::new("https://minio.local:9443/", "b", "us-east-1", creds()).unwrap();
        assert_eq!(client.authority, "minio.local:9443");
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
