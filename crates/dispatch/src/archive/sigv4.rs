//! Minimal AWS SigV4 request signing for the S3 REST API — GET/PUT object
//! only, full (non-chunked, non-streaming) payload signing. Hand-written
//! rather than pulling `aws-sigv4`/`aws-sdk-s3`: `sha2` was already a
//! dispatch dependency (`crate::events::sha256_hex`); `hmac` is the one new
//! crate this adds, and it reuses sha2's own transitive deps
//! (crypto-common/digest/generic-array) — a small addition next to the
//! aws-sdk-s3 + aws-config stack the coordinator flagged as RAM-risky
//! tonight. See `http.rs`'s module doc for the transport this signs for.
//!
//! Reference: <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::events::sha256_hex;

type HmacSha256 = Hmac<Sha256>;

/// Everything one signature needs. `path` is ALREADY the percent-encoded
/// canonical URI (see [`uri_encode_path`]) — callers encode once and reuse
/// the same encoded path for both the signature and the literal request
/// line, so the two can never drift apart.
pub struct SignParams<'a> {
    pub method: &'a str,
    /// Bare authority the request is signed against (`host[:port]`, no
    /// scheme) — must match the literal `Host` header sent on the wire.
    pub host: &'a str,
    pub path: &'a str,
    pub region: &'a str,
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub session_token: Option<&'a str>,
    pub payload: &'a [u8],
    /// `YYYYMMDDTHHMMSSZ` (UTC, no milliseconds) — the caller's injected
    /// clock, never read here (no `SystemTime::now()`/`Utc::now()` inside a
    /// signer: a fixed `now` keeps this testable and keeps the clock seam
    /// where the rest of the crate puts it).
    pub amz_date: &'a str,
}

/// The headers a signed request must carry (in addition to `content-length`,
/// which the HTTP layer sets and which SigV4 does not require signing).
pub struct SignedHeaders {
    pub authorization: String,
    pub x_amz_date: String,
    pub x_amz_content_sha256: String,
    pub x_amz_security_token: Option<String>,
}

pub fn sign(p: &SignParams) -> SignedHeaders {
    let date_stamp = &p.amz_date[0..8]; // YYYYMMDD prefix of YYYYMMDDTHHMMSSZ
    let payload_hash = sha256_hex(p.payload);

    let mut headers: Vec<(&str, String)> = vec![
        ("host", p.host.to_string()),
        ("x-amz-content-sha256", payload_hash.clone()),
        ("x-amz-date", p.amz_date.to_string()),
    ];
    if let Some(token) = p.session_token {
        headers.push(("x-amz-security-token", token.to_string()));
    }
    headers.sort_by_key(|(k, _)| *k);

    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    // No query string (GET/PUT object take none) — the blank line between
    // canonical-headers and signed-headers IS the empty canonical-query-string
    // line per the spec's 6-line canonical request shape.
    let canonical_request = format!(
        "{}\n{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        p.method, p.path,
    );

    let credential_scope = format!("{date_stamp}/{}/s3/aws4_request", p.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{credential_scope}\n{}",
        p.amz_date,
        sha256_hex(canonical_request.as_bytes()),
    );

    let signing_key = derive_signing_key(p.secret_access_key, date_stamp, p.region, "s3");
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        p.access_key_id
    );

    SignedHeaders {
        authorization,
        x_amz_date: p.amz_date.to_string(),
        x_amz_content_sha256: payload_hash,
        x_amz_security_token: p.session_token.map(str::to_string),
    }
}

fn derive_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Percent-encode a full path per S3's "UriEncode" with `encodeSlash=false`:
/// each `/`-delimited segment is encoded independently (unreserved chars —
/// `A-Za-z0-9-._~` — pass through, `/` is preserved as the separator).
/// Both the request line and the signature must use this SAME encoded form
/// (the caller runs it once and reuses the result — see [`SignParams::path`]).
pub fn uri_encode_path(path: &str) -> String {
    path.split('/')
        .map(uri_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn uri_encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_path_preserves_slashes_and_unreserved_chars() {
        assert_eq!(
            uri_encode_path("/bucket/host/claude-code/session-1.jsonl"),
            "/bucket/host/claude-code/session-1.jsonl"
        );
    }

    #[test]
    fn uri_encode_path_encodes_reserved_chars_per_segment() {
        // A divergence set-aside key carries a colon-ish timestamp shape in
        // spirit; exercise a segment with a space and a plus to prove
        // per-byte percent-encoding (not just a no-op on the happy path).
        assert_eq!(uri_encode_path("/a b/c+d"), "/a%20b/c%2Bd");
    }

    #[test]
    fn signature_is_deterministic_for_identical_inputs() {
        let params = |secret: &'static str| SignParams {
            method: "GET",
            host: "127.0.0.1:3900",
            path: "/quorum-transcripts/host/claude-code/s.jsonl",
            region: "us-east-1",
            access_key_id: "AKID",
            secret_access_key: secret,
            session_token: None,
            payload: b"",
            amz_date: "20260707T000000Z",
        };
        let a = sign(&params("secretA"));
        let b = sign(&params("secretA"));
        assert_eq!(a.authorization, b.authorization);
    }

    #[test]
    fn signature_changes_with_the_secret() {
        let params = |secret: &'static str| SignParams {
            method: "GET",
            host: "127.0.0.1:3900",
            path: "/bucket/key",
            region: "us-east-1",
            access_key_id: "AKID",
            secret_access_key: secret,
            session_token: None,
            payload: b"",
            amz_date: "20260707T000000Z",
        };
        let a = sign(&params("secretA"));
        let b = sign(&params("secretB"));
        assert_ne!(a.authorization, b.authorization);
    }

    #[test]
    fn signature_changes_with_the_payload() {
        let params = |payload: &'static [u8]| SignParams {
            method: "PUT",
            host: "127.0.0.1:3900",
            path: "/bucket/key",
            region: "us-east-1",
            access_key_id: "AKID",
            secret_access_key: "secret",
            session_token: None,
            payload,
            amz_date: "20260707T000000Z",
        };
        let a = sign(&params(b"hello"));
        let b = sign(&params(b"world"));
        assert_ne!(a.authorization, b.authorization);
        assert_ne!(a.x_amz_content_sha256, b.x_amz_content_sha256);
    }

    #[test]
    fn session_token_header_present_only_when_given() {
        let base = SignParams {
            method: "GET",
            host: "h",
            path: "/b/k",
            region: "us-east-1",
            access_key_id: "AKID",
            secret_access_key: "secret",
            session_token: None,
            payload: b"",
            amz_date: "20260707T000000Z",
        };
        let without = sign(&base);
        assert!(without.x_amz_security_token.is_none());
        assert!(!without.authorization.contains("x-amz-security-token"));

        let with_token = SignParams {
            session_token: Some("TOKEN"),
            ..base
        };
        let with = sign(&with_token);
        assert_eq!(with.x_amz_security_token.as_deref(), Some("TOKEN"));
        assert!(with.authorization.contains("x-amz-security-token"));
    }

    /// Cross-checked against an independent from-scratch computation using
    /// Python's stdlib `hashlib`/`hmac` (NOT boto3 — a second implementation
    /// of the documented SigV4 algorithm, not a reuse of AWS's own library),
    /// run locally against these exact fixed inputs (see the coordinator's
    /// note on sanity-checking cheaply and locally, no garage/AWS needed for
    /// this). The live wire-format interop against a real store is QA's
    /// separate loop stage.
    #[test]
    fn matches_independently_computed_reference_vector() {
        let signed = sign(&SignParams {
            method: "PUT",
            host: "127.0.0.1:3900",
            path: "/quorum-transcripts/emslima/claude-code/abc123.jsonl",
            region: "us-east-1",
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            session_token: None,
            payload: b"hello archive",
            amz_date: "20260707T153000Z",
        });
        assert_eq!(
            signed.x_amz_content_sha256,
            "f8976760708ac1d60ab4b2dd1fa3c02d3bbf9693846f1db27aa77b46f0bb4276"
        );
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260707/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
             Signature=2a484fa7b981da88c3d543386a92325f7ba20f81fdd75b51a0256f79edfab4bc"
        );
    }
}
